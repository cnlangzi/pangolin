//! IP reputation of the gateway (issue #141).
//!
//! The gateway rejects a client with `429 Too Many Requests` when it
//! exceeds one of the site's limits. That rejection is soft: it tells
//! the client to slow down and it is expected to be temporary. But the
//! gateway has already decided that the client misbehaved, and an
//! independent opinion about the client — is this a hostile address, is
//! this a bot that merely looks like one — is worth having before the
//! rejection is made permanent.
//!
//! The opinion comes from the `badbots` crate. Two signals are used:
//!
//!   * the **address**: `BadBots::assess_ip` answers from the local
//!     state and schedules the upstream verification in the
//!     library's own worker thread. The call performs no network I/O
//!     on the calling thread, so the request path never waits for an
//!     upstream.
//!   * the **user agent**: `BadBots::assess_user_agent` is free — no
//!     network, no quota — and answers whether the client declares
//!     itself a crawler or an automation tool.
//!
//! The library is consulted **only** on the throttle path: a request
//! that passed the rate limiter never reaches it, so the upstream quota
//! is spent on clients that already tripped a limit and on nobody else.
//!
//! ## Precedence
//!
//! A bot the `knownbots` validator confirmed — by user agent, by the
//! publisher's official address ranges and by reverse DNS — is exempt.
//! That verdict is a stronger signal than the reputation of the
//! address the crawler happens to share with others, so such a client
//! is never escalated and the library is never asked about it.
//!
//! ## Where the verdict is kept
//!
//! The verdict the library returns is the only state the escalation
//! keeps. It is not written into the site's IP rules, into the global
//! blacklist or into the penalty box: the library already remembers
//! it, and a second copy would be a second source of truth for the
//! same decision.

use std::net::IpAddr;
use std::sync::Arc;

use badbots::{
    AbuseIpdbConfig, BadBots, BlocklistDeConfig, Classification, Config, CrowdSecConfig,
    DShieldConfig, EvidencePolicy, ProviderConfig, SpamhausConfig,
};
use serde::{Deserialize, Serialize};

use crate::types::{BadbotsEvidencePolicy, BadbotsSettings};

/// What the reputation library said about a client.
///
/// The value is recorded per rejected client so the operator can see
/// *why* a client was escalated on the rate-limit page.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BadbotVerdict {
    /// The address was convicted by the reputation library.
    BadAddress,
    /// The user agent declares a crawler or an automation client.
    BotUserAgent,
    /// The address was convicted **and** the user agent declares a bot.
    BadAddressAndBotUserAgent,
    /// The library was consulted and convicted nothing.
    Unknown,
}

impl BadbotVerdict {
    /// Whether the verdict turns a soft rejection into a hard one.
    ///
    /// Only the three positive verdicts escalate; [`Self::Unknown`]
    /// is the library saying "I looked and found nothing", which
    /// leaves the answer at `429`.
    pub fn escalates(&self) -> bool {
        !matches!(self, BadbotVerdict::Unknown)
    }

    /// A stable, machine-readable code for the verdict.
    ///
    /// The admin UI keys the badge colour off this code, so it has to
    /// be a fixed set of strings — the human-readable label may
    /// change, the code may not.
    pub fn code(&self) -> &'static str {
        match self {
            BadbotVerdict::BadAddress => "bad_address",
            BadbotVerdict::BotUserAgent => "bot_user_agent",
            BadbotVerdict::BadAddressAndBotUserAgent => "bad_address_bot_user_agent",
            BadbotVerdict::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for BadbotVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            BadbotVerdict::BadAddress => "bad address",
            BadbotVerdict::BotUserAgent => "bot user-agent",
            BadbotVerdict::BadAddressAndBotUserAgent => "bad address + bot user-agent",
            BadbotVerdict::Unknown => "unknown",
        };
        f.write_str(text)
    }
}

/// Build the reputation library from the operator's settings.
///
/// The library is built once at startup and rebuilt when the operator
/// saves new settings — a provider key is baked into the library
/// instance when it is built, so a new key needs a new instance.
///
/// The three providers that need no credential (Spamhaus DROP,
/// DShield, the Blocklist.de service lists) are always enabled: they
/// publish a list and ask for nothing. The two providers that need a
/// key (AbuseIPDB, CrowdSec) are enabled **only** when the key is
/// present — the library refuses to build a provider that is enabled
/// without its credential, so enabling one without a key would take
/// the whole feature down.
///
/// A failure is returned to the caller, which logs it and starts
/// without the reputation check: the gateway serves traffic whether or
/// not an optional reputation check is available.
pub fn build_library(settings: &BadbotsSettings) -> Result<Arc<BadBots>, String> {
    BadBots::builder()
        .config(library_config(settings))
        .build()
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

/// The library's own configuration for the operator's settings.
///
/// Split out of [`build_library`] so the mapping — which provider is
/// enabled with which credential — is inspectable without building a
/// library.
pub fn library_config(settings: &BadbotsSettings) -> Config {
    // The credential-free list publishers.
    let spamhaus = SpamhausConfig {
        enabled: true,
        ..Default::default()
    };
    let dshield = DShieldConfig {
        enabled: true,
        ..Default::default()
    };
    let blocklist_de = BlocklistDeConfig {
        enabled: true,
        ..Default::default()
    };

    // The keyed lookup providers — enabled only with a key.
    let abuseipdb = non_empty(settings.abuseipdb_api_key.as_deref()).map(|key| AbuseIpdbConfig {
        enabled: true,
        api_key: Some(badbots::ApiKey::new(key)),
        ..Default::default()
    });
    let crowdsec = non_empty(settings.crowdsec_api_key.as_deref()).map(|key| CrowdSecConfig {
        enabled: true,
        api_key: Some(badbots::ApiKey::new(key)),
        ..Default::default()
    });

    Config {
        providers: ProviderConfig {
            abuseipdb,
            crowdsec,
            spamhaus: Some(spamhaus),
            blocklist_de: Some(blocklist_de),
            dshield: Some(dshield),
        },
        evidence_policy: match settings.evidence_policy {
            BadbotsEvidencePolicy::Strict => EvidencePolicy::Strict,
            BadbotsEvidencePolicy::Lenient => EvidencePolicy::Lenient,
        },
        ..Config::default()
    }
}

/// The empty string is "not configured", not "configured with an empty
/// key" — a form that submits an empty password field must not enable
/// a provider whose key is empty.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Ask the library what it thinks of a client that the rate limiter
/// has just decided to reject.
///
/// Returns `None` when the library was **not** consulted — the client
/// is a verified known bot, the gateway has no reputation check, or
/// the authoritative client IP is unknown. `None` means "no opinion",
/// never "a good opinion".
///
/// The call performs no network I/O on the calling thread: the address
/// verdict comes from the library's local state and the upstream
/// verification it schedules runs in the library's own worker thread.
/// A client whose address has never been looked at therefore answers
/// `Unknown` on its first throttled request and is escalated from the
/// next one onwards.
///
/// `known_bot` is the identity the `knownbots` validator produced. It
/// has priority over both signals: a verified bot is never escalated.
pub fn assess(
    library: Option<&BadBots>,
    client_ip: Option<IpAddr>,
    user_agent: Option<&str>,
    known_bot: Option<&str>,
) -> Option<BadbotVerdict> {
    // A verified known bot is exempt. The library is not asked about
    // it at all, so the upstream quota of every provider stays
    // untouched for the crawlers.
    if known_bot.is_some() {
        return None;
    }
    // Without an authoritative client IP there is nothing to assess
    // and nothing to record: behind a CDN or a load balancer without
    // the configured header, the TCP peer is a proxy hop and must not
    // be judged as the visitor.
    let client_ip = client_ip?;
    let library = library?;

    let bot_user_agent = library.assess_user_agent(user_agent).is_bot;
    let bad_address = library.assess_ip(client_ip).classification == Classification::Bad;

    Some(match (bad_address, bot_user_agent) {
        (true, true) => BadbotVerdict::BadAddressAndBotUserAgent,
        (true, false) => BadbotVerdict::BadAddress,
        (false, true) => BadbotVerdict::BotUserAgent,
        (false, false) => BadbotVerdict::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    use async_trait::async_trait;
    use badbots::{
        Assertion, CoverageLevel, Evidence, EvidenceStrength, MatchScope, ProviderCapability,
        ProviderCost, ProviderError, ProviderFinding, ProviderId, ProviderRuntime, SourceFamily,
        ThreatCategory,
    };

    /// A provider that convicts one address and says nothing about
    /// the rest.
    ///
    /// The stub is what makes the escalation testable without an
    /// upstream: the library asks the provider in process, so no
    /// packet leaves the host and the verdict is deterministic.
    struct StubProvider {
        runtime: Arc<ProviderRuntime>,
    }

    #[async_trait]
    impl badbots::LookupProvider for StubProvider {
        fn id(&self) -> ProviderId {
            ProviderId::new("stub")
        }
        fn family(&self) -> SourceFamily {
            SourceFamily::new("stub")
        }
        fn capability(&self) -> ProviderCapability {
            ProviderCapability::Corroborating
        }
        fn cost(&self) -> ProviderCost {
            ProviderCost::Free
        }
        async fn lookup(&self, ip: IpAddr) -> Result<ProviderFinding, ProviderError> {
            let now = self.runtime.clock().now();
            let evidence = if ip == IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)) {
                vec![Evidence {
                    provider: ProviderId::new("stub"),
                    family: SourceFamily::new("stub"),
                    assertion: Assertion::Attack,
                    category: ThreatCategory::WebAttack,
                    upstream_category: None,
                    strength: EvidenceStrength::Convicting,
                    scope: MatchScope::Exact,
                    observed_at: Some(now),
                    published_at: Some(now),
                    retrieved_at: Some(now),
                    // No expiry: the observation stays valid for the
                    // lifetime of the test.
                    expires_at: None,
                    observation_window: None,
                    count: Some(1),
                    upstream_values: Default::default(),
                }]
            } else {
                Vec::new()
            };
            Ok(ProviderFinding {
                provider: ProviderId::new("stub"),
                coverage: CoverageLevel::Partial,
                evidence,
                suppression: None,
                expires_at: None,
            })
        }
    }

    /// A library that convicts `8.8.8.8` and nothing else.
    ///
    /// The address has to be routable: the library refuses to assess a
    /// loopback, private or documentation address, so the RFC 5737
    /// test addresses every other test in this repo uses would answer
    /// `Unknown` forever.
    fn stub_library() -> Arc<BadBots> {
        let config = Config {
            evidence_policy: EvidencePolicy::Lenient,
            ..Config::default()
        };
        Arc::new(
            BadBots::builder()
                .config(config)
                .lookup_provider(badbots::daily_guard(100), |runtime| StubProvider {
                    runtime,
                })
                .build()
                .expect("the stub library builds"),
        )
    }

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn a_library_without_providers_never_convicts() {
        let library = BadBots::builder()
            .config(Config::default())
            .build()
            .expect("builds");
        let verdict = assess(Some(&library), Some(ip(1, 1, 1, 1)), Some("curl/8.0"), None);
        assert_eq!(verdict, Some(BadbotVerdict::BotUserAgent));
    }
    #[test]
    fn a_verified_known_bot_is_never_escalated() {
        // Knownbots has priority: the library is not asked at all, so
        // even a bot user agent stays a soft rejection.
        let library = stub_library();
        let verdict = assess(
            Some(&library),
            Some(ip(8, 8, 8, 8)),
            Some("Googlebot/2.1"),
            Some("googlebot"),
        );
        assert_eq!(verdict, None);
    }

    #[test]
    fn an_unknown_client_ip_is_never_escalated() {
        let library = stub_library();
        assert_eq!(assess(Some(&library), None, Some("curl/8.0"), None), None);
    }

    #[test]
    fn a_gateway_without_the_library_never_escalates() {
        assert_eq!(
            assess(None, Some(ip(8, 8, 8, 8)), Some("curl/8.0"), None),
            None
        );
    }

    #[test]
    fn a_browser_user_agent_is_not_a_bot() {
        let library = BadBots::builder()
            .config(Config::default())
            .build()
            .expect("builds");
        let verdict = assess(
            Some(&library),
            Some(ip(1, 1, 1, 1)),
            Some(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36",
            ),
            None,
        );
        assert_eq!(verdict, Some(BadbotVerdict::Unknown));
        assert!(!verdict.unwrap().escalates());
    }

    #[test]
    fn a_crawler_user_agent_is_a_bot() {
        let library = BadBots::builder()
            .config(Config::default())
            .build()
            .expect("builds");
        let verdict = assess(Some(&library), Some(ip(1, 1, 1, 1)), Some("curl/8.0"), None);
        assert_eq!(verdict, Some(BadbotVerdict::BotUserAgent));
        assert!(verdict.unwrap().escalates());
    }

    #[tokio::test]
    async fn a_convicted_address_escalates_once_the_verification_ran() {
        // The first assessment of an address answers from the local
        // state, which knows nothing about it yet: the answer is
        // `Unknown` and the verification is scheduled in the
        // background. The next assessment of the same address reports
        // the conviction.
        let library = stub_library();
        let first = assess(Some(&library), Some(ip(8, 8, 8, 8)), Some("curl/8.0"), None);
        assert_eq!(first, Some(BadbotVerdict::BotUserAgent));

        let verified = library.verify_ip(ip(8, 8, 8, 8)).await;
        assert_eq!(verified.classification, Classification::Bad);

        let second = assess(Some(&library), Some(ip(8, 8, 8, 8)), Some("curl/8.0"), None);
        assert_eq!(second, Some(BadbotVerdict::BadAddressAndBotUserAgent));
        assert!(second.unwrap().escalates());
    }

    #[test]
    fn a_library_without_a_keyed_provider_is_built_without_it() {
        let settings = BadbotsSettings::default();
        // The credential-free publishers are always there.
        let config = library_config(&settings);
        assert!(config.providers.spamhaus_enabled());
        assert!(config.providers.dshield_enabled());
        assert!(config.providers.blocklist_de_enabled());
        // The keyed ones are not.
        assert!(!config.providers.abuseipdb_enabled());
        assert!(!config.providers.crowdsec_enabled());
        // And the library builds without any key.
        build_library(&settings).expect("builds without any key");
    }

    #[test]
    fn a_keyed_provider_is_enabled_when_its_key_is_present() {
        let settings = BadbotsSettings {
            abuseipdb_api_key: Some("secret".into()),
            ..BadbotsSettings::default()
        };
        let config = library_config(&settings);
        assert!(config.providers.abuseipdb_enabled());
        assert!(!config.providers.crowdsec_enabled());
        build_library(&settings).expect("builds with a key");
    }

    #[test]
    fn an_empty_key_does_not_enable_its_provider() {
        let settings = BadbotsSettings {
            abuseipdb_api_key: Some("   ".into()),
            ..BadbotsSettings::default()
        };
        let config = library_config(&settings);
        assert!(!config.providers.abuseipdb_enabled());
    }

    #[test]
    fn the_library_is_built_with_the_lenient_policy_by_default() {
        let settings = BadbotsSettings::default();
        assert_eq!(
            library_config(&settings).evidence_policy,
            EvidencePolicy::Lenient
        );
        let strict = BadbotsSettings {
            evidence_policy: BadbotsEvidencePolicy::Strict,
            ..BadbotsSettings::default()
        };
        assert_eq!(
            library_config(&strict).evidence_policy,
            EvidencePolicy::Strict
        );
    }
}
