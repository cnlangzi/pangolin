//! The standalone HTTP listener.
//!
//! The API has its **own** listener — its own port, its own router,
//! its own authentication. Nothing of the admin UI's cookie session,
//! CSRF or HTML rendering is reachable from here.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::server::{ApiRequest, ApiState, route};

/// Serve the API until `shutdown` is cancelled.
///
/// The listener is plain HTTP: the API is meant to run behind the
/// dashboard's own TLS-terminating proxy, and the node does not
/// implement TLS for it (issue #185).
pub async fn serve(
    addr: SocketAddr,
    state: Arc<ApiState>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    log::info!("api: listening on {local}");

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                log::info!("api: shutting down");
                break;
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!("api: accepting a connection failed: {e}");
                        continue;
                    }
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(handle(state, req, peer).await) }
                    });
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        log::debug!("api: the connection ended: {e}");
                    }
                });
            }
        }
    }
    Ok(())
}

/// Turn one HTTP request into one API request.
async fn handle(
    state: Arc<ApiState>,
    req: hyper::Request<hyper::body::Incoming>,
    peer: SocketAddr,
) -> hyper::Response<Full<Bytes>> {
    let method = req.method().as_str().to_string();
    // The request target as sent — path **and** query string. This is
    // the string the signature covers, so it must not be normalised.
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let headers = req.headers().clone();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();

    let response = route(
        &state,
        ApiRequest {
            method,
            path,
            headers,
            body,
            peer: Some(peer),
        },
    )
    .await;

    hyper::Response::builder()
        .status(response.status)
        .header("Content-Type", response.content_type)
        .body(Full::new(Bytes::from(response.body)))
        .unwrap_or_else(|_| {
            hyper::Response::builder()
                .status(500)
                .body(Full::new(Bytes::from_static(b"")))
                .expect("the fallback response builds")
        })
}
