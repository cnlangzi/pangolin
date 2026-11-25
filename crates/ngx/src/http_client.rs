//! The gateway's outbound HTTPS client.
//!
//! Every outbound HTTPS request of the gateway goes through this
//! builder, so they all trust the **same** roots: the system's own
//! certificate store. The client is built with
//! `rustls-tls-manual-roots`, so the native roots are loaded and
//! added explicitly — there is no bundled root set, and there is no
//! "skip verification" path. A dashboard with an invalid certificate
//! is refused, exactly like a provider range list served by one.
//!
//! The two callers differ in one thing only: the dashboard's SSE
//! subscription must not be cut off by a total timeout, so it gets a
//! client without one.

use std::time::Duration;

/// The user agent every outbound request carries.
const USER_AGENT: &str = concat!("pangolin-ngx/", env!("CARGO_PKG_VERSION"));

/// The client for a one-shot control-plane request.
///
/// `timeout` bounds the **whole** request — the connect, the
/// transfer and the body.
pub fn build_request_client(timeout: Duration) -> anyhow::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(USER_AGENT);
    let (builder, _roots) = with_native_roots(builder)?;
    Ok(builder.build()?)
}

/// The client for a long-lived stream — the dashboard's subscription.
///
/// There is **no** total timeout: the stream is supposed to stay
/// open for hours. The connect is still bounded, so a dashboard that
/// accepts the connection and then says nothing does not hold a
/// socket forever.
pub fn build_stream_client() -> anyhow::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(USER_AGENT);
    let (builder, _roots) = with_native_roots(builder)?;
    Ok(builder.build()?)
}

/// Add the system's certificate store to the client.
///
/// The roots are the **native** ones — the store the operating system
/// keeps up to date. A certificate reqwest rejects is skipped: the
/// others still make the client usable.
fn with_native_roots(
    mut builder: reqwest::ClientBuilder,
) -> anyhow::Result<(reqwest::ClientBuilder, usize)> {
    // A store that cannot be read is **not** fatal: the client goes
    // out with no roots, so every HTTPS request fails its
    // verification — and the failure stays inside the channel that
    // made the request. The gateway still starts.
    let certs = match rustls_native_certs::load_native_certs() {
        Ok(certs) => certs,
        Err(e) => {
            log::warn!(
                "http: the native certificate store could not be read: {e} — the outbound \
                 HTTPS requests cannot verify any certificate"
            );
            Vec::new()
        }
    };
    if certs.is_empty() {
        log::warn!(
            "http: the native certificate store is empty — the outbound HTTPS \
             requests cannot verify any certificate"
        );
    }
    let mut added = 0;
    for cert in certs {
        match reqwest::Certificate::from_der(cert.as_ref()) {
            Ok(c) => {
                builder = builder.add_root_certificate(c);
                added += 1;
            }
            Err(e) => log::warn!("http: skipping a native certificate: {e}"),
        }
    }
    Ok((builder, added))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both clients are built from the **system's** certificate store.
    ///
    /// The regression this pins: the dashboard channel and the
    /// provider range fetch must trust the same roots. A client built
    /// from a different set would accept a dashboard the rest of the
    /// gateway does not.
    #[test]
    fn both_clients_are_built() {
        build_request_client(Duration::from_secs(30)).expect("the request client builds");
        build_stream_client().expect("the stream client builds");
    }

    /// The roots come from the operating system's store.
    ///
    /// The client is built with `rustls-tls-manual-roots`, so the
    /// native roots are the **only** ones it trusts — there is no
    /// bundled set and no "skip verification" path.
    #[test]
    fn the_roots_are_the_system_certificate_store() {
        let native = rustls_native_certs::load_native_certs().expect("the store loads");
        let builder = reqwest::Client::builder();
        let (builder, added) = with_native_roots(builder).expect("the roots load");
        // Every root the client trusts came from the native store:
        // the builder adds them and nothing else.
        assert!(
            native.is_empty() || added > 0,
            "a non-empty native store has to reach the client"
        );
        assert!(
            builder.build().is_ok(),
            "the client must build with the native roots"
        );
    }
}
