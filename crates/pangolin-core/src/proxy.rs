//! Reverse-proxy policy shared by `ngx` and `tun`.
//!
//! **Layering**: this module sits between "site config" (higher) and
//! "transport" (lower). It does not know which transport (pingora
//! server, a tunnel stream, …) ultimately carries the request —
//! callers supply transport-specific executors that consume a
//! [`HttpRequest`] and return a [`HttpResponse`].
//!
//! **Single source of truth for Host rewriting**: [`apply_proxy_policy`]
//! is the only function in the workspace that mutates the `Host`
//! header (and adds `X-Forwarded-*`). Since issue #153 it runs on the
//! gateway only — the tunnel is a byte splice, so the tun has nothing
//! to apply the policy to.
//!
//! See `docs/design/reverse-proxy.md` for the full v8 design.

use std::path::PathBuf;

use crate::types::HostMode;
use crate::{ParseError, parse_backend};

// ── HTTP model ──────────────────────────────────────────────────
//
// The request/response shapes the gateway's proxy path works with.
// They are the gateway's model, not the tunnel's protocol: the tunnel
// is a byte splice (issue #153), so nothing in the tunnel speaks
// HTTP. They used to live in the tunnel module — with the protocol
// they were re-homed here when the protocol moved to the published
// `pangolin-tun` crate (issue #159).

/// A request, as the proxy path sees it: the head fields plus the
/// body it may already have buffered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A response, as the proxy path builds it: the status line, the
/// headers and the (possibly buffered) body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub version: String,
    pub status_line: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Strip RFC 7230 §6.1 hop-by-hop headers.
///
/// Called by [`apply_proxy_policy`] — the only caller that re-writes
/// the request on its way upstream. The tunnel is a byte splice
/// (issue #153), so nothing re-serialises the request bytes on the
/// tun's side any more.
pub fn strip_hop_by_hop_headers(headers: &mut Vec<(String, String)>) {
    fn is_hop_by_hop(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) || lower.starts_with("proxy-")
    }
    headers.retain(|(k, _)| !is_hop_by_hop(k));
}

// ── Scheme ──────────────────────────────────────────────────────

/// The scheme the client used to reach the gateway. Forwarded as
/// `X-Forwarded-Proto` when `apply_proxy_policy` rewrites the Host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scheme {
    #[default]
    Http,
    Https,
}

// ── ProxyCtx ────────────────────────────────────────────────────

/// Per-request policy context, built by the caller (ngx or tun).
///
/// `original_host` is preserved across the whole pipeline so that
/// `X-Forwarded-Host` can echo it back to the backend.
#[derive(Debug, Clone, Default)]
pub struct ProxyCtx {
    pub original_host: String,
    pub original_scheme: Scheme,
    pub host_mode: HostMode,
    pub host_custom: Option<String>,
    /// The TCP peer address (e.g. `203.0.113.42`) — the host
    /// pangolin accepted the connection from. When set, the
    /// proxy policy appends it to `X-Forwarded-For` (or starts
    /// the chain if absent). It is **not** the client's IP:
    /// behind a CDN / LB the peer is a proxy hop, and the
    /// visitor's address travels in the `P-Client-Ip` header
    /// instead (see `docs/design/gateway-headers.md`).
    ///
    /// Both direct and tunnel paths populate this from
    /// `session.client_addr()` on the ngx side; the tunnel
    /// path also ships it inside the tunnel request so the
    /// tun's `apply_proxy_policy` call sees it.
    pub peer_ip: Option<String>,
}

// ── BackendTarget ──────────────────────────────────────────────

/// Disambiguated backend target, produced by [`parse_backend_to_target`].
///
/// `Http` and `Https` carry the host:port to connect to and the
/// `base_path` (the part of the URL after the authority) that
/// should be **prepended** to the request URI before sending.
/// `base_path` is the part of the backend URL **before** any
/// `?query`; it is the request-side path-prefix that turns
/// `GET /chat` into `GET <base>/chat`.
///
/// `File` carries the on-disk root. The path component of the
/// request URI is joined with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendTarget {
    Http {
        host: String,
        port: u16,
        base_path: String,
    },
    Https {
        host: String,
        port: u16,
        base_path: String,
    },
    File {
        doc_root: PathBuf,
    },
}

impl BackendTarget {
    /// True iff this target's `host_mode`-aware code path goes
    /// through [`serve_file_target`] instead of an HTTP client.
    pub fn is_file(&self) -> bool {
        matches!(self, BackendTarget::File { .. })
    }

    /// `host:port` (or `0.0.0.0:0` for file backends, which never
    /// dial). Caller can use this for logging / metrics.
    pub fn authority(&self) -> String {
        match self {
            BackendTarget::Http { host, port, .. } | BackendTarget::Https { host, port, .. } => {
                format!("{}:{}", host, port)
            }
            BackendTarget::File { doc_root } => doc_root.display().to_string(),
        }
    }

    /// True iff this target uses TLS when dialing.
    pub fn is_tls(&self) -> bool {
        matches!(self, BackendTarget::Https { .. })
    }
}

// ── parse_backend_to_target ───────────────────────────────────

/// Wrap [`parse_backend`] (which returns a flat `tun_name:url`
/// pair) into a typed return.
///
/// The returned `BackendTarget` carries the **host, port, and
/// base_path** parsed out of the URL. `tun_name` is the routing
/// prefix — empty means "direct", non-empty means "route through
/// the named tun".
pub fn parse_backend_to_target(backend: &str) -> Result<(String, BackendTarget), ParseError> {
    let (tun_name, url) = parse_backend(backend)?;

    if let Some(stripped) = url.strip_prefix("file://") {
        // `parse_backend` already validated the scheme; just turn
        // the URL into a PathBuf.
        return Ok((
            tun_name,
            BackendTarget::File {
                doc_root: PathBuf::from(stripped),
            },
        ));
    }

    let (scheme_sep, rest) = if let Some(pos) = url.find("://") {
        (pos, &url[pos + 3..])
    } else {
        return Err(ParseError::UnsupportedScheme(url.to_string()));
    };
    let scheme = &url[..scheme_sep];

    // `rest` is `host[:port][/base_path]`
    let (authority, base_path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], rest[idx..].to_string()),
        None => (rest, String::new()),
    };
    let (host, port) = parse_authority(authority)
        .ok_or_else(|| ParseError::UnsupportedScheme(format!("bad authority in {url:?}")))?;

    let target = match scheme {
        "http" => BackendTarget::Http {
            host: host.to_string(),
            port,
            base_path,
        },
        "https" => BackendTarget::Https {
            host: host.to_string(),
            port,
            base_path,
        },
        _ => return Err(ParseError::UnsupportedScheme(url.to_string())),
    };
    Ok((tun_name, target))
}

fn parse_authority(s: &str) -> Option<(&str, u16)> {
    // IPv6: bracketed form `[..]:port`. The closing `]` is
    // guaranteed to appear before the port separator (RFC 3986).
    if let Some(stripped) = s.strip_prefix('[') {
        let end = stripped.find(']')?;
        let host = &stripped[..end];
        let after = &stripped[end + 1..];
        let port: u16 = after.strip_prefix(':')?.parse().ok()?;
        return Some((host, port));
    }
    // Try host:port format for IPv4/hostname
    if let Some(colon) = s.rfind(':') {
        let host = &s[..colon];
        let port = s[colon + 1..].parse::<u16>().ok()?;
        // Reject bare IPv6 (no brackets) by checking if the host
        // portion contains colons — ambiguous and unsupported.
        if host.contains(':') {
            return None;
        }
        Some((host, port))
    } else {
        Some((s, 80))
    }
}

// ── apply_proxy_policy ─────────────────────────────────────────

/// Apply per-site policy to a request **in place**.
///
/// **INVARIANT**: this function never mutates `request.target`,
/// `request.method`, or `request.body`. It only mutates `headers`.
///
/// What it does, in order:
///   1. Strip RFC 7230 §6.1 hop-by-hop headers.
///   2. Rewrite `Host` per `ctx.host_mode`.
///   3. Inject the `X-Forwarded-*` family on **every** path
///      variant (direct + tun, all host_modes) so the
///      upstream always sees the hop path:
///        - `X-Forwarded-For`: append `ctx.peer_ip` to any
///          existing chain (RFC 7239); if absent, just the
///          peer IP.
///        - `X-Forwarded-Proto`: `http` or `https` per
///          `ctx.original_scheme`.
///        - `X-Forwarded-Host`: the public `Host` the client
///          used.
///
/// The visitor's own address travels in the `P-Client-Ip`
/// header instead (see `docs/design/gateway-headers.md`).
///
/// Caller is responsible for constructing `request.target` (the
/// path-prefix concat happens before this is called) and for
/// attaching the right `BackendTarget` to the chosen transport.
pub fn apply_proxy_policy(request: &mut HttpRequest, ctx: &ProxyCtx) {
    // 1. Hop-by-hop
    strip_hop_by_hop_headers(&mut request.headers);

    // 2-3. Host rewrite and X-Forwarded-*
    apply_host_and_forwarded_headers(request, ctx);
}

/// Apply only Host rewrite and X-Forwarded-* headers, without
/// stripping hop-by-hop headers. Used by ngx direct proxy path
/// where pingora handles hop-by-hop headers automatically.
pub fn apply_proxy_policy_without_hop_by_hop_stripping(request: &mut HttpRequest, ctx: &ProxyCtx) {
    apply_host_and_forwarded_headers(request, ctx);
}

fn apply_host_and_forwarded_headers(request: &mut HttpRequest, ctx: &ProxyCtx) {
    // 1. Host rewrite per host_mode. `Passthrough` is a no-op for
    // Host (we leave whatever the client sent — the executor
    // doesn't need to know about the rewrite in that case).
    // `Backend` and `Custom` set the Host to a deterministic
    // value. The executor still has the final say: when
    // `host_mode == Backend`, the executor will overwrite Host
    // with the backend's actual authority (host:port) since the
    // proxy_ctx only carries `original_host`. When `Custom`, the
    // executor copies the value the policy just set.
    match ctx.host_mode {
        HostMode::Passthrough => {
            // No-op for Host header.
        }
        HostMode::Backend => {
            // We can't pick the backend authority here (it lives
            // in BackendTarget, not in ProxyCtx). The executor
            // does that. But we still want to add
            // X-Forwarded-*; both Backend and Custom do that
            // below.
        }
        HostMode::Custom => {
            let value = ctx
                .host_custom
                .as_ref()
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| ctx.original_host.clone());
            upsert_header(&mut request.headers, "Host", &value);
        }
    }

    // 2. X-Forwarded-* — apply on EVERY host_mode (including
    // Passthrough) so upstreams can always see the hop path.
    // The headers are only meaningful when the client actually
    // went through this proxy; for direct internal traffic
    // (test fixtures), `ctx.peer_ip` is None and the loop
    // degrades to "no X-Forwarded-For" — the other two
    // (X-Forwarded-Proto, X-Forwarded-Host) are still set
    // because they're cheap and useful.
    //
    // Only the TCP peer is appended: the resolved client IP
    // travels in the `P-Client-Ip` header, never here. Two
    // headers claiming to be the client IP is exactly the
    // inconsistency the gateway-header contract removes.
    if let Some(ip) = ctx.peer_ip.as_deref().filter(|s| !s.is_empty()) {
        append_xff_chain(&mut request.headers, ip);
    }
    if !ctx.original_host.is_empty() {
        upsert_header(&mut request.headers, "X-Forwarded-Host", &ctx.original_host);
    }
    let proto = match ctx.original_scheme {
        Scheme::Http => "http",
        Scheme::Https => "https",
    };
    upsert_header(&mut request.headers, "X-Forwarded-Proto", proto);
}

/// Append `ip` to the existing `X-Forwarded-For` chain. If the
/// header is absent, sets it to `ip` outright. Uses
/// comma-separated values per RFC 7239 §5.2 / the de-facto
/// `X-Forwarded-For` convention.
fn append_xff_chain(headers: &mut Vec<(String, String)>, ip: &str) {
    if let Some(slot) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case("X-Forwarded-For"))
    {
        // Append only if the chain doesn't already end with
        // this IP (avoid duplicates if a buggy upstream added
        // us to the chain and re-emitted).
        let existing = slot.1.clone();
        let last = existing.rsplit(',').next().map(|s| s.trim()).unwrap_or("");
        if last != ip {
            slot.1 = format!("{}, {}", existing, ip);
        }
        return;
    }
    headers.push(("X-Forwarded-For".to_string(), ip.to_string()));
}

/// Set or replace a header (case-insensitive name match).
fn upsert_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(slot) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
    {
        slot.1 = value.to_string();
        return;
    }
    headers.push((name.to_string(), value.to_string()));
}

// ── is_streaming_request ──────────────────────────────────────

/// The token that marks a request as a streaming one.
///
/// It lives here so that the rule has exactly one home: both
/// [`is_streaming_request`] and the gateway's own per-header check
/// compare against this one constant.
const STREAMING_MARKER: &[u8] = b"text/event-stream";

/// The header names the streaming marker is looked for in.
///
/// The names are lower-case — that is how an `http::HeaderMap`
/// stores them — but the comparison in [`is_streaming_header`] is
/// case-insensitive, so a request may spell them any way.
///
/// The gateway asks the request's header map for exactly these two
/// names, so a header that cannot carry the marker is never even
/// converted to a string.
pub const STREAMING_HEADER_NAMES: [&str; 2] = ["accept", "content-type"];

/// Whether one request header marks the request as a streaming one.
///
/// The header **name** is compared case-insensitively (a request may
/// spell it `Accept` or `ACCEPT`), the **value** as a
/// case-insensitive **substring** — the browser sends
/// `*/*, text/event-stream`.
///
/// The comparison allocates nothing: the gateway runs it on every
/// candidate header of every request, and a lower-cased copy of the
/// value would cost one allocation per candidate header.
pub fn is_streaming_header(name: &str, value: &str) -> bool {
    if !STREAMING_HEADER_NAMES
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
    {
        return false;
    }
    value
        .as_bytes()
        .windows(STREAMING_MARKER.len())
        .any(|window| window.eq_ignore_ascii_case(STREAMING_MARKER))
}

/// Detect if `request` expects a streaming response that should
/// bypass the standard HttpResponse buffering path.
///
/// Heuristics (conservative — only matches things we can be
/// confident are streaming):
///   1. `Accept: text/event-stream` — the canonical SSE marker.
///   2. `Content-Type: text/event-stream` — some backends echo
///      this on the request side (less common, but harmless).
///
/// Future extensions (chunked responses, custom headers, path
/// patterns) can be added here without touching the transport
/// layer. The flag only decides how the response is *accounted*
/// for — since issue #153 both the direct and the tunnelled path
/// stream the response natively.
///
/// This is the framework-neutral **whole-request** entry point: it
/// takes the owned [`HttpRequest`] the framework-neutral code works
/// with. The gateway's request path does not go through it — it
/// applies [`is_streaming_header`] to the headers it already holds
/// (issue #217) — so the rule still has exactly one home.
pub fn is_streaming_request(request: &HttpRequest) -> bool {
    request
        .headers
        .iter()
        .any(|(name, value)| is_streaming_header(name, value))
}

// ── serve_file_target ─────────────────────────────────────────

/// Serve a static file from `doc_root` in response to `request`.
///
/// **Aligned with nginx's `root` + `index` directives:**
///   - Join `request.path` with `doc_root`.
///   - Reject any `..` segment (path traversal guard).
///   - Reject hidden files (basename starting with `.`).
///   - Directory request → try `index.html` then `index.htm`.
///   - 404 on missing (no `try_files` fallback).
///   - 304 on `If-None-Match` (ETag) / `If-Modified-Since` (mtime).
///   - Honours `Range` requests (single-range, in-memory body).
///   - Sets `Content-Type` from extension via `mime_guess`.
///   - Sets `ETag` (mtime+size) and `Last-Modified` headers.
///
/// `request.path` is the URI path component (e.g. `/foo/bar.html`),
/// already the request's path (the caller is responsible for
/// stripping any `host_mode`-derived path-prefix logic — that
/// happens in the caller, not here).
pub fn serve_file_target(request: &HttpRequest, doc_root: &std::path::Path) -> HttpResponse {
    serve_file_target_impl(request, doc_root)
}

// Internal entry point; kept separate so unit tests can hit it
// without pulling the public re-export.
fn serve_file_target_impl(request: &HttpRequest, doc_root: &std::path::Path) -> HttpResponse {
    // Extract the path component from the request target. Targets
    // come in two shapes:
    //   - origin form: "/foo/bar.html"
    //   - absolute form: "http://host/foo/bar.html"
    // For file:// backends the target is always origin form (the
    // HTTP server / yamux client rewrites it before we get here).
    let path = request
        .target
        .split_once("://")
        .map(|(_, rest)| {
            // absolute form: skip authority, then take path
            match rest.find('/') {
                Some(idx) => &rest[idx..],
                None => "/",
            }
        })
        .unwrap_or(request.target.as_str());

    // Path traversal guard
    if path.split('?').next().unwrap_or(path).contains("..") {
        return synth_error(400, "Bad Request: path contains '..'");
    }

    // Join with doc_root. Path is URL-encoded; we don't decode it
    // here because we treat the path as a literal filesystem path
    // (matches the existing ngx-side `serve_static_file`).
    let rel = path.trim_start_matches('/');
    let fs_path = if rel.is_empty() {
        doc_root.to_path_buf()
    } else {
        doc_root.join(rel)
    };

    let meta = match std::fs::metadata(&fs_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Directory: try index.html / index.htm.
            if fs_path.is_dir() || rel.is_empty() {
                for idx in &["index.html", "index.htm"] {
                    let candidate = fs_path.join(idx);
                    if let Ok(m) = std::fs::metadata(&candidate) {
                        return serve_file(&candidate, m, request, true);
                    }
                }
            }
            return synth_error(404, "Not Found");
        }
        Err(e) => {
            return synth_error(500, &format!("metadata error: {e}"));
        }
    };

    if meta.is_dir() {
        for idx in &["index.html", "index.htm"] {
            let candidate = fs_path.join(idx);
            if let Ok(m) = std::fs::metadata(&candidate) {
                return serve_file(&candidate, m, request, true);
            }
        }
        return synth_error(404, "Not Found");
    }

    // Hidden file guard
    if let Some(name) = fs_path.file_name().and_then(|n| n.to_str())
        && name.starts_with('.')
    {
        return synth_error(403, "Forbidden: hidden file");
    }

    serve_file(&fs_path, meta, request, false)
}

fn serve_file(
    fs_path: &std::path::Path,
    meta: std::fs::Metadata,
    request: &HttpRequest,
    is_index: bool,
) -> HttpResponse {
    use std::time::SystemTime;

    let mime = mime_guess::from_path(fs_path)
        .first_or_octet_stream()
        .to_string();
    let mtime = meta.modified().ok();
    let etag = mtime.map(|t| {
        let dur = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        format!("\"{}x{}\"", meta.len(), dur.as_secs())
    });

    // 304 short-circuit
    if !is_index {
        if let Some(etag_val) = &etag
            && let Some((_, inm)) = request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("If-None-Match"))
            && (inm == etag_val.as_str() || inm == "*")
        {
            let mut headers = vec![
                ("ETag".to_string(), etag_val.clone()),
                ("Content-Type".to_string(), mime),
            ];
            if let Some(mt) = mtime {
                headers.push(("Last-Modified".to_string(), httpdate::fmt_http_date(mt)));
            }
            return HttpResponse {
                version: "HTTP/1.1".into(),
                status_line: "304 Not Modified".into(),
                headers,
                body: Vec::new(),
            };
        }
        if let Some(mt) = mtime
            && let Some((_, ims)) = request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("If-Modified-Since"))
            && let Ok(ims_dt) = httpdate::parse_http_date(ims)
            && mt <= ims_dt
        {
            let headers = vec![
                ("Last-Modified".to_string(), httpdate::fmt_http_date(mt)),
                ("Content-Type".to_string(), mime),
            ];
            return HttpResponse {
                version: "HTTP/1.1".into(),
                status_line: "304 Not Modified".into(),
                headers,
                body: Vec::new(),
            };
        }
    }

    // Read content
    let body = match std::fs::read(fs_path) {
        Ok(b) => b,
        Err(e) => return synth_error(500, &format!("read error: {e}")),
    };

    // Range support (single-range, bytes=START-END)
    let (status_line, body, content_length) =
        if let Some((start, end)) = parse_range(&body.len(), &request.headers) {
            let slice = body[start..=end].to_vec();
            let total = body.len();
            let mut headers = vec![
                ("Content-Type".to_string(), mime.clone()),
                (
                    "Content-Range".to_string(),
                    format!("bytes {}-{}/{}", start, end, total),
                ),
                ("Accept-Ranges".to_string(), "bytes".to_string()),
                ("Content-Length".to_string(), slice.len().to_string()),
            ];
            if let Some(et) = &etag {
                headers.push(("ETag".to_string(), et.clone()));
            }
            if let Some(mt) = mtime {
                headers.push(("Last-Modified".to_string(), httpdate::fmt_http_date(mt)));
            }
            return HttpResponse {
                version: "HTTP/1.1".into(),
                status_line: "206 Partial Content".into(),
                headers,
                body: slice,
            };
        } else {
            ("200 OK".to_string(), body.clone(), body.len().to_string())
        };

    let mut headers = vec![
        ("Content-Type".to_string(), mime),
        ("Content-Length".to_string(), content_length),
        // Match nginx's default for `location` blocks without an
        // explicit `expires` directive — force revalidation per
        // request, don't let downstream caches serve stale copies.
        ("Cache-Control".to_string(), "no-cache".to_string()),
    ];
    if let Some(et) = &etag {
        headers.push(("ETag".to_string(), et.clone()));
    }
    if let Some(mt) = mtime {
        headers.push(("Last-Modified".to_string(), httpdate::fmt_http_date(mt)));
    }

    HttpResponse {
        version: "HTTP/1.1".into(),
        status_line,
        headers,
        body,
    }
}

fn parse_range(total: &usize, headers: &[(String, String)]) -> Option<(usize, usize)> {
    let (_, range_hdr) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Range"))?;
    let value = range_hdr.trim().strip_prefix("bytes=")?;
    let (start_s, end_s) = value.split_once('-')?;
    let start: usize = start_s.parse().ok()?;
    let end: usize = if end_s.is_empty() {
        total.saturating_sub(1)
    } else {
        end_s.parse().ok()?
    };
    if start > end || end >= *total {
        return None;
    }
    Some((start, end))
}

fn synth_error(status: u16, reason: &str) -> HttpResponse {
    let body = reason.as_bytes().to_vec();
    HttpResponse {
        version: "HTTP/1.1".into(),
        status_line: format!("{status} {reason}"),
        headers: vec![
            (
                "Content-Type".to_string(),
                "text/plain; charset=utf-8".into(),
            ),
            ("Content-Length".to_string(), body.len().to_string()),
            ("Connection".to_string(), "close".into()),
        ],
        body,
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            target: target.into(),
            version: "HTTP/1.1".into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
        }
    }

    // I-1
    #[test]
    fn apply_proxy_policy_never_touches_path() {
        let original = "/chat?x=1";
        for mode in [HostMode::Passthrough, HostMode::Backend, HostMode::Custom] {
            let mut req = mk("GET", original, &[("Host", "dev.yaitoo.cn")], &[]);
            let ctx = ProxyCtx {
                original_host: "dev.yaitoo.cn".into(),
                original_scheme: Scheme::Http,
                host_mode: mode,
                host_custom: Some("custom.example.com".into()),
                peer_ip: None,
            };
            apply_proxy_policy(&mut req, &ctx);
            assert_eq!(req.target, original, "path touched under {:?}", mode);
        }
    }

    // I-2
    #[test]
    fn apply_proxy_policy_backend_mode_passthrough_xfh() {
        // host_mode=Backend does NOT mutate Host directly here —
        // that is the executor's job (it knows the backend
        // authority). But it MUST add X-Forwarded-Host and
        // X-Forwarded-Proto.
        let mut req = mk("GET", "/", &[("Host", "dev.yaitoo.cn")], &[]);
        let ctx = ProxyCtx {
            original_host: "dev.yaitoo.cn".into(),
            original_scheme: Scheme::Http,
            host_mode: HostMode::Backend,
            host_custom: None,
            peer_ip: None,
        };
        apply_proxy_policy(&mut req, &ctx);
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "X-Forwarded-Host" && v == "dev.yaitoo.cn")
        );
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "X-Forwarded-Proto" && v == "http")
        );
        // Host unchanged
        assert_eq!(
            req.headers.iter().find(|(k, _)| k == "Host").unwrap().1,
            "dev.yaitoo.cn"
        );
    }

    // I-3
    #[test]
    fn apply_proxy_policy_custom_mode() {
        let mut req = mk("GET", "/", &[("Host", "dev.yaitoo.cn")], &[]);
        let ctx = ProxyCtx {
            original_host: "dev.yaitoo.cn".into(),
            original_scheme: Scheme::Https,
            host_mode: HostMode::Custom,
            host_custom: Some("custom.example.com".into()),
            peer_ip: None,
        };
        apply_proxy_policy(&mut req, &ctx);
        assert_eq!(
            req.headers.iter().find(|(k, _)| k == "Host").unwrap().1,
            "custom.example.com"
        );
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "X-Forwarded-Host" && v == "dev.yaitoo.cn")
        );
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "X-Forwarded-Proto" && v == "https")
        );
    }

    // I-4
    #[test]
    fn apply_proxy_policy_passthrough_leaves_host() {
        let mut req = mk("GET", "/", &[("Host", "dev.yaitoo.cn")], &[]);
        let ctx = ProxyCtx {
            original_host: "dev.yaitoo.cn".into(),
            original_scheme: Scheme::Http,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            peer_ip: None,
        };
        apply_proxy_policy(&mut req, &ctx);
        // The I-4 invariant: Passthrough leaves Host alone.
        assert_eq!(
            req.headers.iter().find(|(k, _)| k == "Host").unwrap().1,
            "dev.yaitoo.cn"
        );
        // Even in Passthrough, the policy injects
        // X-Forwarded-Host + X-Forwarded-Proto so the upstream
        // always knows the public host + scheme. This is the
        // behavior post-#XFF work; the previous assertion
        // ("no X-Forwarded-* in Passthrough") is the old
        // artifact we deliberately removed because nginx-style
        // backends inspect these headers regardless of host_mode.
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "X-Forwarded-Host" && v == "dev.yaitoo.cn"),
            "X-Forwarded-Host must be set even in Passthrough"
        );
        assert!(
            req.headers
                .iter()
                .any(|(k, v)| k == "X-Forwarded-Proto" && v == "http"),
            "X-Forwarded-Proto must be set even in Passthrough"
        );
        // No peer_ip → no X-Forwarded-For (we can't make up an
        // IP that isn't there).
        assert!(!req.headers.iter().any(|(k, _)| k == "X-Forwarded-For"));
        // `X-Real-IP` is gone for good (issue #115): the client IP
        // has exactly one name now — `P-Client-Ip`.
        assert!(!req.headers.iter().any(|(k, _)| k == "X-Real-IP"));
    }

    /// `peer_ip = Some(...)` triggers the X-Forwarded-For
    /// injection. The XFF chain is APPENDED (not replaced) so
    /// multi-hop setups preserve upstream proxy entries. This is
    /// the I-21 / I-22 behavior at the `apply_proxy_policy`
    /// level; the e2e layer (`tests/`) covers the wire-level
    /// flow.
    #[test]
    fn apply_proxy_policy_appends_peer_ip_to_xff_chain() {
        // 1. Fresh request (no XFF yet) → policy starts the chain.
        let mut req = mk("GET", "/", &[("Host", "dev.yaitoo.cn")], &[]);
        let ctx = ProxyCtx {
            original_host: "dev.yaitoo.cn".into(),
            original_scheme: Scheme::Http,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            peer_ip: Some("203.0.113.42".into()),
        };
        apply_proxy_policy(&mut req, &ctx);
        let xff = req
            .headers
            .iter()
            .find(|(k, _)| k == "X-Forwarded-For")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert_eq!(
            xff, "203.0.113.42",
            "fresh request: chain starts with the peer IP"
        );
        // `X-Real-IP` is never emitted (issue #115).
        assert!(
            !req.headers.iter().any(|(k, _)| k == "X-Real-IP"),
            "X-Real-IP must not be emitted on any path"
        );

        // 2. Existing chain (upstream proxy already added an
        //    entry) → policy appends.
        let mut req = mk(
            "GET",
            "/",
            &[
                ("Host", "dev.yaitoo.cn"),
                ("X-Forwarded-For", "198.51.100.1"),
            ],
            &[],
        );
        apply_proxy_policy(&mut req, &ctx);
        let xff = req
            .headers
            .iter()
            .find(|(k, _)| k == "X-Forwarded-For")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert_eq!(
            xff, "198.51.100.1, 203.0.113.42",
            "existing chain: append, not replace"
        );

        // 3. Duplicate (peer_ip already at the chain tail) →
        //    policy does NOT add a second occurrence.
        let mut req = mk(
            "GET",
            "/",
            &[
                ("Host", "dev.yaitoo.cn"),
                ("X-Forwarded-For", "198.51.100.1, 203.0.113.42"),
            ],
            &[],
        );
        apply_proxy_policy(&mut req, &ctx);
        let xff = req
            .headers
            .iter()
            .find(|(k, _)| k == "X-Forwarded-For")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert_eq!(
            xff, "198.51.100.1, 203.0.113.42",
            "duplicate tail: no double-append"
        );
    }

    // I-5
    #[test]
    fn apply_proxy_policy_strips_hop_by_hop() {
        let mut req = mk(
            "GET",
            "/",
            &[
                ("Host", "dev.yaitoo.cn"),
                ("Connection", "close"),
                ("Keep-Alive", "timeout=5"),
                ("Proxy-Authorization", "Bearer x"),
                ("TE", "trailers"),
                ("Trailer", "Expires"),
                ("Transfer-Encoding", "chunked"),
                ("Upgrade", "h2c"),
            ],
            &[],
        );
        let ctx = ProxyCtx {
            original_host: "dev.yaitoo.cn".into(),
            original_scheme: Scheme::Http,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            peer_ip: None,
        };
        apply_proxy_policy(&mut req, &ctx);
        let names: Vec<&str> = req.headers.iter().map(|(k, _)| k.as_str()).collect();
        for hop in [
            "Connection",
            "Keep-Alive",
            "Proxy-Authorization",
            "TE",
            "Trailer",
            "Transfer-Encoding",
            "Upgrade",
        ] {
            assert!(!names.contains(&hop), "hop-by-hop {hop} survived");
        }
    }

    // I-6
    #[test]
    fn parse_backend_to_target_roundtrips() {
        // direct http
        let (tun, t) = parse_backend_to_target("http://127.0.0.1:8080").unwrap();
        assert_eq!(tun, "");
        match t {
            BackendTarget::Http {
                host,
                port,
                base_path,
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 8080);
                assert_eq!(base_path, "");
            }
            _ => panic!("expected Http"),
        }
        // direct https
        let (tun, t) = parse_backend_to_target("https://x.example.com:443").unwrap();
        assert_eq!(tun, "");
        match t {
            BackendTarget::Https { host, port, .. } => {
                assert_eq!(host, "x.example.com");
                assert_eq!(port, 443);
            }
            _ => panic!("expected Https"),
        }
        // file
        let (tun, t) = parse_backend_to_target("file:///var/www/static").unwrap();
        assert_eq!(tun, "");
        match t {
            BackendTarget::File { doc_root } => {
                assert_eq!(doc_root, PathBuf::from("/var/www/static"));
            }
            _ => panic!("expected File"),
        }
        // tunnel + http
        let (tun, t) = parse_backend_to_target("office:http://10.0.0.1:9000").unwrap();
        assert_eq!(tun, "office");
        match t {
            BackendTarget::Http { host, port, .. } => {
                assert_eq!(host, "10.0.0.1");
                assert_eq!(port, 9000);
            }
            _ => panic!("expected Http"),
        }
        // tunnel + http with path
        let (tun, t) = parse_backend_to_target("office:http://10.0.0.1:9000/blogs").unwrap();
        assert_eq!(tun, "office");
        match t {
            BackendTarget::Http { base_path, .. } => {
                assert_eq!(base_path, "/blogs");
            }
            _ => panic!("expected Http"),
        }
        // unsupported scheme
        assert!(parse_backend_to_target("office:mailto:foo@bar.com").is_err());
    }

    // is_streaming_request: heuristics for SSE / streaming responses.
    // Used by ngx to mark the request as a stream, so the traffic
    // histogram ignores the hang time of a long-lived response.
    #[test]
    fn is_streaming_request_detects_text_event_stream() {
        // Accept header matches → streaming
        let req = mk("GET", "/events", &[("Accept", "text/event-stream")], &[]);
        assert!(is_streaming_request(&req));

        // Mixed-case value still matches
        let req = mk("GET", "/events", &[("Accept", "TEXT/EVENT-STREAM")], &[]);
        assert!(is_streaming_request(&req));

        // Accept with extra params (the browser default form)
        let req = mk(
            "GET",
            "/events",
            &[("Accept", "*/*, text/event-stream")],
            &[],
        );
        assert!(is_streaming_request(&req));

        // application/json is NOT streaming
        let req = mk("GET", "/api", &[("Accept", "application/json")], &[]);
        assert!(!is_streaming_request(&req));

        // No Accept header at all
        let req = mk("GET", "/", &[("Host", "x.test")], &[]);
        assert!(!is_streaming_request(&req));
    }

    // The per-header predicate the whole heuristic is made of. The
    // gateway runs it on the headers it already holds, so the truth
    // table here is the contract both entry points share.
    #[test]
    fn is_streaming_header_matches_the_marker_on_accept_or_content_type_only() {
        // The canonical marker, on either header name.
        assert!(is_streaming_header("Accept", "text/event-stream"));
        assert!(is_streaming_header("Content-Type", "text/event-stream"));

        // The header name is matched case-insensitively.
        assert!(is_streaming_header("accept", "text/event-stream"));
        assert!(is_streaming_header("ACCEPT", "text/event-stream"));
        assert!(is_streaming_header("content-type", "text/event-stream"));

        // The value is matched case-insensitively and as a substring
        // (the browser sends `*/*, text/event-stream`).
        assert!(is_streaming_header("Accept", "TEXT/EVENT-STREAM"));
        assert!(is_streaming_header("Accept", "*/*, text/event-stream"));

        // The marker in another header is not a streaming request.
        assert!(!is_streaming_header(
            "X-Accel-Buffering",
            "text/event-stream"
        ));

        // Another value on a streaming header name is not the marker.
        assert!(!is_streaming_header("Accept", "application/json"));

        // A value shorter than the marker cannot contain it.
        assert!(!is_streaming_header("Accept", "text/event"));

        // An empty value is not the marker.
        assert!(!is_streaming_header("Accept", ""));
    }

    // I-8
    #[test]
    fn serve_file_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let req = mk("GET", "/../escape", &[], &[]);
        let resp = serve_file_target(&req, dir.path());
        assert_eq!(resp.status_line, "400 Bad Request: path contains '..'");
    }

    // I-9
    #[test]
    fn serve_file_rejects_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden"), b"secret").unwrap();
        let req = mk("GET", "/.hidden", &[], &[]);
        let resp = serve_file_target(&req, dir.path());
        assert!(resp.status_line.starts_with("403"));
    }

    // I-10
    #[test]
    fn serve_file_404_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        let req = mk("GET", "/nope", &[], &[]);
        let resp = serve_file_target(&req, dir.path());
        assert!(resp.status_line.starts_with("404"));
    }

    // I-11
    #[test]
    fn serve_file_index_html() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), b"<h1>hi</h1>").unwrap();
        let req = mk("GET", "/", &[], &[]);
        let resp = serve_file_target(&req, dir.path());
        assert_eq!(resp.status_line, "200 OK");
        assert_eq!(resp.body, b"<h1>hi</h1>");
    }

    // I-12
    #[test]
    fn serve_file_etag_304() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let meta = std::fs::metadata(dir.path().join("a.txt")).unwrap();
        let mtime = meta.modified().unwrap();
        let dur = mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let etag = format!("\"{}x{}\"", meta.len(), dur.as_secs());
        let req = mk("GET", "/a.txt", &[("If-None-Match", &etag)], &[]);
        let resp = serve_file_target(&req, dir.path());
        assert_eq!(resp.status_line, "304 Not Modified");
        assert!(resp.body.is_empty());
    }

    // I-13
    #[test]
    fn serve_file_range() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"0123456789").unwrap();
        let req = mk("GET", "/a.txt", &[("Range", "bytes=2-5")], &[]);
        let resp = serve_file_target(&req, dir.path());
        assert_eq!(resp.status_line, "206 Partial Content");
        assert_eq!(resp.body, b"2345");
    }

    // I-14
    #[test]
    fn serve_file_mime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.html"), b"<h1>hi</h1>").unwrap();
        let req = mk("GET", "/a.html", &[], &[]);
        let resp = serve_file_target(&req, dir.path());
        let ct = resp
            .headers
            .iter()
            .find(|(k, _)| k == "Content-Type")
            .unwrap();
        assert!(ct.1.contains("text/html"));
    }
}
