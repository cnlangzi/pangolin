//! The freshness of a cached response.
//!
//! Two things decide it (issue #259): what the **origin** asked for
//! and what the operator's `cache.ttl` allows.
//!
//! * `Cache-Control: no-store` or `private` — the response is not
//!   cacheable at all.
//! * `Cache-Control: no-cache` — the response *is* stored, but it is
//!   stale from the moment it lands: the next read revalidates it
//!   against the origin.
//! * `s-maxage` / `max-age` — the origin's freshness, capped by
//!   `cache.ttl`.
//! * no directive at all — `cache.ttl`.
//!
//! `stale-while-revalidate` and `stale-if-error` are the origin's
//! tolerance windows and are carried into the metadata as they are:
//! they say how long the origin is willing to be wrong, which is not
//! the gateway's call to shorten.

use std::time::{Duration, SystemTime};

use pingora::cache::cache_control::CacheControl;

/// The freshness a stored response gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// When the response stops being fresh.
    pub fresh_until: SystemTime,
    /// How long the response may be served stale while it is
    /// revalidated in the background.
    pub stale_while_revalidate_sec: u32,
    /// How long the response may be served stale when the origin
    /// fails.
    pub stale_if_error_sec: u32,
}

/// The freshness a response gets, given the origin's directives and
/// the gateway's default TTL.
///
/// `ttl` is the cap: an origin may not make a response live longer
/// than the operator allows.
pub fn resolve(now: SystemTime, cache_control: Option<&CacheControl>, ttl: Duration) -> Freshness {
    let stale_while_revalidate_sec = cache_control
        .and_then(|cc| cc.stale_while_revalidate().ok().flatten())
        .unwrap_or(0);
    let stale_if_error_sec = cache_control
        .and_then(|cc| cc.stale_if_error().ok().flatten())
        .unwrap_or(0);

    let fresh_until = match cache_control {
        // `no-cache` means "store it, but always ask again". The
        // entry is already stale when it lands: the freshness is
        // anchored one second in the past, so the next read finds it
        // stale and revalidates it.
        Some(cc) if cc.no_cache() => now.checked_sub(Duration::from_secs(1)).unwrap_or(now),
        // The origin's own freshness, capped by the operator's TTL.
        Some(cc) => match cc
            .s_maxage()
            .ok()
            .flatten()
            .or_else(|| cc.max_age().ok().flatten())
        {
            Some(seconds) => now + Duration::from_secs((seconds as u64).min(ttl.as_secs())),
            None => now + ttl,
        },
        None => now + ttl,
    };

    Freshness {
        fresh_until,
        stale_while_revalidate_sec,
        stale_if_error_sec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::CACHE_CONTROL;
    use pingora::http::ResponseHeader;

    fn control(value: &str) -> CacheControl {
        let mut header = ResponseHeader::build(200, None).expect("build header");
        header
            .insert_header(CACHE_CONTROL, value)
            .expect("insert cache-control");
        CacheControl::from_resp_headers(&header).expect("parse cache-control")
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn an_origin_without_directives_gets_the_default_ttl() {
        let now = at(1_000);
        let freshness = resolve(now, None, Duration::from_secs(60));
        assert_eq!(freshness.fresh_until, at(1_060));
    }

    #[test]
    fn the_origins_freshness_is_used() {
        let now = at(1_000);
        let freshness = resolve(now, Some(&control("max-age=30")), Duration::from_secs(60));
        assert_eq!(freshness.fresh_until, at(1_030));
    }

    #[test]
    fn the_origins_freshness_is_capped_by_the_ttl() {
        let now = at(1_000);
        let freshness = resolve(now, Some(&control("max-age=600")), Duration::from_secs(60));
        assert_eq!(freshness.fresh_until, at(1_060));
    }

    #[test]
    fn the_shared_freshness_wins_over_the_private_one() {
        let now = at(1_000);
        let freshness = resolve(
            now,
            Some(&control("max-age=600, s-maxage=30")),
            Duration::from_secs(60),
        );
        assert_eq!(freshness.fresh_until, at(1_030));
    }

    #[test]
    fn a_no_cache_response_is_stored_but_is_already_stale() {
        let now = at(1_000);
        let freshness = resolve(now, Some(&control("no-cache")), Duration::from_secs(60));
        assert!(
            freshness.fresh_until < now,
            "a no-cache response must land stale"
        );
    }

    #[test]
    fn the_stale_windows_are_carried_over() {
        let now = at(1_000);
        let freshness = resolve(
            now,
            Some(&control(
                "max-age=30, stale-while-revalidate=5, stale-if-error=60",
            )),
            Duration::from_secs(60),
        );
        assert_eq!(freshness.stale_while_revalidate_sec, 5);
        assert_eq!(freshness.stale_if_error_sec, 60);
    }

    #[test]
    fn a_response_without_the_stale_directives_has_no_stale_window() {
        let now = at(1_000);
        let freshness = resolve(now, Some(&control("max-age=30")), Duration::from_secs(60));
        assert_eq!(freshness.stale_while_revalidate_sec, 0);
        assert_eq!(freshness.stale_if_error_sec, 0);
    }
}
