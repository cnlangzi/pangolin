//! End-to-end check that every embedded bot still verifies against a
//! published address.
//!
//! URL-backed and custom-CIDR bots go through [`Validator::refresh`]
//! (the production download path, certificate checks disabled) and
//! must come back [`VerifyStatus::Verified`]. Where a vendor publishes
//! a stable address, that exact address is pinned. The rest must
//! verify an address taken from the list just downloaded, so a parser
//! or TLS regression that leaves the prefix set empty fails here.
//!
//! RDNS-only bots with a confirmed PTR must verify that address after
//! the background lookup. Bots with no stable public address must
//! still match the User-Agent and fail closed (`Pending`), so a
//! broken marker cannot pass silently.
//!
//! Run: `cargo test -p knownbots --test known_ips`

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use knownbots::{Bot, Validator, VerifyStatus, load_bots};

/// Pinned published address. `None` means "no single stable address":
/// URL bots then use an address from the refreshed prefix set, and
/// RDNS bots must fail closed as `Pending`.
struct Pin {
    name: &'static str,
    ip: Option<&'static str>,
}

/// Official list addresses captured from the vendor documents.
/// Google's `66.249.64.1` sits in `66.249.64.0/27`, which Google
/// publishes for common crawlers (Googlebot and the bots that share
/// that file).
const URL_PINS: &[Pin] = &[
    Pin {
        name: "adsbot",
        ip: None,
    },
    Pin {
        name: "adsbot-mobile",
        ip: None,
    },
    Pin {
        name: "ahrefsbot",
        ip: Some("5.39.1.224"),
    },
    Pin {
        name: "amazonbot",
        ip: Some("3.81.245.78"),
    },
    Pin {
        name: "apis-google",
        ip: None,
    },
    Pin {
        name: "applebot",
        ip: Some("17.241.208.161"),
    },
    Pin {
        name: "applebot-extended",
        ip: Some("17.241.208.161"),
    },
    Pin {
        name: "bingbot",
        ip: Some("157.55.39.1"),
    },
    Pin {
        name: "claudebot",
        ip: Some("216.73.216.1"),
    },
    Pin {
        name: "cloudflare-alwaysonline",
        ip: Some("173.245.48.1"),
    },
    Pin {
        name: "cloudflare-healthchecks",
        ip: Some("173.245.48.1"),
    },
    Pin {
        name: "duckduckbot",
        ip: None,
    },
    Pin {
        name: "facebookexternalhit",
        ip: Some("31.13.24.10"),
    },
    Pin {
        name: "feedfetcher",
        ip: None,
    },
    Pin {
        name: "google-extended",
        ip: Some("66.249.64.1"),
    },
    Pin {
        name: "google-inspectiontool",
        ip: Some("66.249.64.1"),
    },
    Pin {
        name: "google-storebot",
        ip: Some("66.249.64.1"),
    },
    Pin {
        name: "googlebot",
        ip: Some("66.249.64.1"),
    },
    Pin {
        name: "googleother",
        ip: Some("66.249.64.1"),
    },
    Pin {
        name: "gptbot",
        ip: Some("132.196.86.1"),
    },
    Pin {
        name: "mediapartners-google",
        ip: None,
    },
    Pin {
        name: "oai-searchbot",
        ip: Some("135.234.64.1"),
    },
    Pin {
        name: "pingdom",
        ip: Some("3.10.222.182"),
    },
    Pin {
        name: "uptimerobot",
        ip: Some("3.12.251.153"),
    },
];

/// RDNS addresses whose PTR and forward DNS both confirm the IP
/// (`crawl.baidu.com`, `spider.yandex.com`, `bot.semrush.com`,
/// `fwd.linkedin.com`, `fbsv.net`). Bytespider's PTR does not resolve
/// forward, and Pinterest's name resolves to IPv6 only, so those stay
/// fail-closed pins.
const RDNS_PINS: &[Pin] = &[
    Pin {
        name: "baiduspider",
        ip: Some("220.181.108.94"),
    },
    Pin {
        name: "bytespider",
        ip: None,
    },
    Pin {
        name: "ccbot",
        ip: None,
    },
    Pin {
        name: "chatgpt-user",
        ip: None,
    },
    Pin {
        name: "discordbot",
        ip: None,
    },
    Pin {
        name: "linkedinbot",
        ip: Some("108.174.10.10"),
    },
    Pin {
        name: "meta-externalagent",
        ip: Some("66.220.149.10"),
    },
    Pin {
        name: "mj12bot",
        ip: None,
    },
    Pin {
        name: "perplexity-user",
        ip: None,
    },
    Pin {
        name: "perplexitybot",
        ip: None,
    },
    Pin {
        name: "petalbot",
        ip: None,
    },
    Pin {
        name: "pinterestbot",
        ip: None,
    },
    Pin {
        name: "redditbot",
        ip: None,
    },
    Pin {
        name: "semrushbot",
        ip: Some("85.208.96.202"),
    },
    Pin {
        name: "semrushbot-ba",
        ip: Some("85.208.96.202"),
    },
    Pin {
        name: "slackbot",
        ip: None,
    },
    Pin {
        name: "sogou",
        ip: None,
    },
    Pin {
        name: "telegrambot",
        ip: None,
    },
    Pin {
        name: "twitterbot",
        ip: None,
    },
    Pin {
        name: "whatsapp",
        ip: None,
    },
    Pin {
        name: "yandexbot",
        ip: Some("5.255.250.1"),
    },
];

fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn claim(marker: &str) -> String {
    format!("Mozilla/5.0 (compatible; {marker}/1.0)")
}

fn pins_by_name(pins: &[Pin]) -> HashMap<&str, &Pin> {
    let mut map = HashMap::new();
    for pin in pins {
        assert!(
            map.insert(pin.name, pin).is_none(),
            "duplicate pin {}",
            pin.name
        );
    }
    map
}

fn embedded_bots() -> Vec<Bot> {
    load_bots(None).expect("embedded bot configs")
}

#[tokio::test]
async fn url_bots_verify_known_ips() {
    install_crypto();
    let bots = embedded_bots();
    let pins = pins_by_name(URL_PINS);
    let ip_bots: Vec<&Bot> = bots
        .iter()
        .filter(|b| !b.urls.is_empty() || !b.custom.is_empty())
        .collect();
    assert_eq!(
        ip_bots.len(),
        pins.len(),
        "URL/custom bot set drifted from URL_PINS"
    );
    for bot in &ip_bots {
        assert!(
            pins.contains_key(bot.name.as_str()),
            "missing known-IP pin for {}",
            bot.name
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let v = Validator::new_sync_only(dir.path()).unwrap();
    v.refresh().await;

    let mut errors = Vec::new();
    for bot in &ip_bots {
        let pin = pins[bot.name.as_str()];
        if v.covered_ip(&bot.name).is_none() {
            errors.push(format!(
                "{}: refresh produced no prefixes (download or parser failed)",
                bot.name
            ));
            continue;
        }
        let ip = match pin.ip {
            Some(raw) => raw.parse::<IpAddr>().expect("pin ip"),
            None => v.covered_ip(&bot.name).unwrap(),
        };
        let ua = claim(&bot.ua);
        let result = v.verify(&ua, ip);
        if !result.is_verified() || result.name != bot.name {
            errors.push(format!(
                "{}: {ip} via {ua:?} -> {:?} name {:?}",
                bot.name, result.status, result.name
            ));
        }
    }

    let forged = v.verify(&claim("Googlebot"), "203.0.113.50".parse().unwrap());
    if forged.status == VerifyStatus::Verified {
        errors.push("forged 203.0.113.50 verified as Googlebot".into());
    }

    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[tokio::test]
async fn rdns_bots_verify_known_ips() {
    install_crypto();
    let bots = embedded_bots();
    let by_name: HashMap<&str, &Bot> = bots.iter().map(|b| (b.name.as_str(), b)).collect();
    let pins = pins_by_name(RDNS_PINS);
    let rdns_bots: Vec<&Bot> = bots
        .iter()
        .filter(|b| b.rdns && b.urls.is_empty() && b.custom.is_empty())
        .collect();
    assert_eq!(
        rdns_bots.len(),
        pins.len(),
        "RDNS bot set drifted from RDNS_PINS"
    );
    for bot in &rdns_bots {
        assert!(
            pins.contains_key(bot.name.as_str()),
            "missing known-IP pin for {}",
            bot.name
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let v = Validator::new_sync_only(dir.path()).unwrap();
    assert!(v.start_background(), "rdns workers must start");

    let mut errors = Vec::new();
    for pin in RDNS_PINS {
        if pin.ip.is_some() {
            continue;
        }
        let bot = by_name[pin.name];
        let result = v.verify(&claim(&bot.ua), "203.0.113.50".parse().unwrap());
        if result.status != VerifyStatus::Pending {
            errors.push(format!(
                "{}: unpinned RDNS claim should fail closed as Pending, got {:?}",
                pin.name, result.status
            ));
        }
    }

    let mut waiting: Vec<&Pin> = RDNS_PINS.iter().filter(|p| p.ip.is_some()).collect();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !waiting.is_empty() && Instant::now() < deadline {
        let mut still = Vec::new();
        for pin in waiting {
            let bot = by_name[pin.name];
            let ip: IpAddr = pin.ip.unwrap().parse().unwrap();
            let result = v.verify(&claim(&bot.ua), ip);
            if result.is_verified() {
                if result.name != pin.name {
                    errors.push(format!("{}: {ip} verified as {}", pin.name, result.name));
                }
            } else if result.status == VerifyStatus::Failed {
                errors.push(format!(
                    "{}: {ip} RDNS rejected (PTR did not match configured domains)",
                    pin.name
                ));
            } else {
                still.push(pin);
            }
        }
        waiting = still;
        if !waiting.is_empty() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    for pin in waiting {
        errors.push(format!(
            "{}: {} still unverified after RDNS wait",
            pin.name,
            pin.ip.unwrap()
        ));
    }
    v.shutdown();

    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
