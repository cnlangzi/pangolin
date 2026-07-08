//! Reverse-proxy policy + frame format shared by `ngx` and `tun`.
//!
//! **Layering**: this module sits between "site config" (higher) and
//! "transport" (lower). It does not know which transport (pingora
//! server, yamux, reqwest, …) ultimately carries the request —
//! callers supply transport-specific executors that consume a
//! [`HttpRequest`] and return a [`HttpResponse`].
//!
//! **Single source of truth for Host rewriting**: [`apply_proxy_policy`]
//! is the only function in the workspace that mutates the `Host`
//! header (and adds `X-Forwarded-*`). Both `ngx` and `tun` call it.
//!
//! See `docs/design/reverse-proxy.md` for the full v8 design.

use std::io::{Error, ErrorKind, Result as IoResult};
use std::path::PathBuf;

use crate::tunnel::{HttpRequest, HttpResponse, encode_http_request, strip_hop_by_hop_headers};
use crate::types::HostMode;
use crate::{ParseError, parse_backend};

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
    /// Client source IP (e.g. `203.0.113.42`). When set, the
    /// proxy policy appends it to `X-Forwarded-For` (or starts
    /// the chain if absent) and sets `X-Real-IP`. Both direct
    /// and tunnel paths populate this from
    /// `session.client_addr()` on the ngx side; the tunnel
    /// path also ships it inside the `TunnelHttpFrame` so the
    /// tun's `apply_proxy_policy` call sees it.
    pub client_ip: Option<String>,
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
///      upstream always sees the real client:
///        - `X-Forwarded-For`: append `ctx.client_ip` to any
///          existing chain (RFC 7239); if absent, just the
///          client IP.
///        - `X-Forwarded-Proto`: `http` or `https` per
///          `ctx.original_scheme`.
///        - `X-Forwarded-Host`: the public `Host` the client
///          used.
///        - `X-Real-IP`: same as `X-Forwarded-For`'s last
///          token (no chain), the bare client IP. The
///          single-value variant is what most nginx-style
///          upstreams inspect.
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
    // Passthrough) so upstreams can always see the real client.
    // The headers are only meaningful when the client actually
    // went through this proxy; for direct internal traffic
    // (test fixtures), `ctx.client_ip` is None and the loop
    // degrades to "no X-Forwarded-For, no X-Real-IP" — the
    // other two (X-Forwarded-Proto, X-Forwarded-Host) are
    // still set because they're cheap and useful.
    if let Some(ip) = ctx.client_ip.as_deref().filter(|s| !s.is_empty()) {
        append_xff_chain(&mut request.headers, ip);
        upsert_header(&mut request.headers, "X-Real-IP", ip);
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

// ── TunnelHttpFrame ────────────────────────────────────────────

/// Per-request payload sent from `ngx` to `tun` over a yamux
/// stream. Carries the entire `HttpRequest`, the routing
/// policy `tun` needs to apply (`host_mode`, `host_custom`),
/// the disambiguated backend target (so the tun knows
/// which host/port to dial or which doc_root to serve from
/// without re-deriving it from the request target), and a
/// flag indicating whether the request is a WebSocket
/// upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelHttpFrame {
    pub request: HttpRequest,
    pub target: BackendTarget,
    pub host_mode: HostMode,
    pub host_custom: Option<String>,
    pub is_upgrade: bool,
    /// True for streaming responses (SSE, long-lived chunked responses).
    /// When set, the tun side bypasses HttpResponse buffering and
    /// relays bytes directly from backend TCP to the yamux stream.
    pub is_streaming: bool,
    /// Client source IP, detected by ngx from `session.client_addr()`.
    /// Shipped to the tun so `apply_proxy_policy` on the tun side
    /// can inject `X-Forwarded-For` + `X-Real-IP` even when the
    /// request goes through a tunnel. `None` for frames that pre-date
    /// the field's introduction; those frames are decoded back as
    /// `None` so old tun versions stay wire-compatible.
    pub client_ip: Option<String>,
}

const FRAME_HOST_PASSTHROUGH: u8 = 0;
const FRAME_HOST_BACKEND: u8 = 1;
const FRAME_HOST_CUSTOM: u8 = 2;
const FRAME_TARGET_HTTP: u8 = 0;
const FRAME_TARGET_HTTPS: u8 = 1;
const FRAME_TARGET_FILE: u8 = 2;

/// Encode a `TunnelHttpFrame` to wire bytes.
///
/// Wire layout:
/// ```text
/// ┌──────────┬──────────┬─────────────────────────────────────┐
/// │ host_mode│ custom?  │ host_custom bytes (when custom? = 1)│
/// │ 1 byte   │ 1 byte   │ 2-byte BE length + UTF-8            │
/// ├──────────┴──────────┴─────────────────────────────────────┤
/// │ is_upgrade (1 byte, 0/1)                                  │
/// │ is_streaming (1 byte, 0/1)                                │
/// ├────────────────────────────────────────────────────────────┤
/// │ target_kind (1 byte: 0=http, 1=https, 2=file)            │
/// │ target_host_len (2 bytes BE) | UTF-8 ("" for file)        │
/// │ target_port (2 bytes BE; 0 for file)                       │
/// │ target_base_path_len (2 bytes BE) | UTF-8 (base_path)      │
/// │ target_doc_root_len (2 bytes BE) | UTF-8 (only for file)  │
/// ├────────────────────────────────────────────────────────────┤
/// │ client_ip_present (1 byte, 0/1)                            │
/// │ client_ip_len (2 bytes BE) | UTF-8 (when present)         │
/// ├────────────────────────────────────────────────────────────┤
/// │ encode_http_request(&request) — full HTTP/1.1 byte stream  │
/// └────────────────────────────────────────────────────────────┘
/// ```
///
/// `client_ip` was added after the initial wire format. The
/// decoder accepts the **old** layout (no `client_ip` block) and
/// decodes back `client_ip = None`, so a new ngx talking to an
/// old tun (and vice-versa) still works on the bytes they each
/// understand.
pub fn encode_tunnel_frame(frame: &TunnelHttpFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + 16 + frame.request.body.len());
    // host_mode
    out.push(host_mode_byte(frame.host_mode));
    // host_custom
    match &frame.host_custom {
        None => out.push(0),
        Some(s) => {
            out.push(1);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
        }
    }
    // is_upgrade
    out.push(if frame.is_upgrade { 1 } else { 0 });
    // is_streaming (feature flag: 0 = not streaming, 1 = streaming)
    out.push(if frame.is_streaming { 1 } else { 0 });
    // target
    let (kind, host, port, base_path, doc_root) = match &frame.target {
        BackendTarget::Http {
            host,
            port,
            base_path,
        } => (
            FRAME_TARGET_HTTP,
            host.clone(),
            *port,
            base_path.clone(),
            String::new(),
        ),
        BackendTarget::Https {
            host,
            port,
            base_path,
        } => (
            FRAME_TARGET_HTTPS,
            host.clone(),
            *port,
            base_path.clone(),
            String::new(),
        ),
        BackendTarget::File { doc_root } => (
            FRAME_TARGET_FILE,
            String::new(),
            0u16,
            String::new(),
            doc_root.to_string_lossy().into_owned(),
        ),
    };
    out.push(kind);
    let host_bytes = host.as_bytes();
    out.extend_from_slice(&(host_bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(host_bytes);
    out.extend_from_slice(&port.to_be_bytes());
    let base_bytes = base_path.as_bytes();
    out.extend_from_slice(&(base_bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(base_bytes);
    let doc_bytes = doc_root.as_bytes();
    out.extend_from_slice(&(doc_bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(doc_bytes);
    // client_ip (optional; see wire-format doc above)
    match &frame.client_ip {
        None => out.push(0),
        Some(s) => {
            out.push(1);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            out.extend_from_slice(bytes);
        }
    }
    // request bytes
    out.extend_from_slice(&encode_http_request(&frame.request));
    out
}

/// Decode a `TunnelHttpFrame` from wire bytes. Inverse of
/// [`encode_tunnel_frame`].
pub fn decode_tunnel_frame(bytes: &[u8]) -> IoResult<TunnelHttpFrame> {
    if bytes.len() < 3 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame header truncated",
        ));
    }
    let host_mode = match bytes[0] {
        FRAME_HOST_PASSTHROUGH => HostMode::Passthrough,
        FRAME_HOST_BACKEND => HostMode::Backend,
        FRAME_HOST_CUSTOM => HostMode::Custom,
        other => {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("unknown host_mode byte 0x{other:02x}"),
            ));
        }
    };
    let custom_present = bytes[1];
    let mut cursor = 2usize;
    let host_custom = if custom_present == 1 {
        if bytes.len() < cursor + 2 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "tunnel frame: missing host_custom length",
            ));
        }
        let len = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]) as usize;
        cursor += 2;
        if bytes.len() < cursor + len {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "tunnel frame: host_custom truncated",
            ));
        }
        let s = std::str::from_utf8(&bytes[cursor..cursor + len])
            .map_err(|e| Error::new(ErrorKind::InvalidData, format!("host_custom utf-8: {e}")))?
            .to_string();
        cursor += len;
        Some(s)
    } else if custom_present != 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("tunnel frame: bad custom_present byte 0x{custom_present:02x}"),
        ));
    } else {
        None
    };
    // is_upgrade
    if bytes.len() < cursor + 1 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: missing is_upgrade byte",
        ));
    }
    let is_upgrade = bytes[cursor] != 0;
    cursor += 1;
    // is_streaming
    if bytes.len() < cursor + 1 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: missing is_streaming byte",
        ));
    }
    let is_streaming = bytes[cursor] != 0;
    cursor += 1;
    // target
    if bytes.len() < cursor + 1 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: missing target kind",
        ));
    }
    let kind = bytes[cursor];
    cursor += 1;
    if bytes.len() < cursor + 2 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: missing host len",
        ));
    }
    let host_len = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]) as usize;
    cursor += 2;
    if bytes.len() < cursor + host_len {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: host truncated",
        ));
    }
    let host = std::str::from_utf8(&bytes[cursor..cursor + host_len])
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("host utf-8: {e}")))?
        .to_string();
    cursor += host_len;
    if bytes.len() < cursor + 2 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: missing port",
        ));
    }
    let port = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]);
    cursor += 2;
    if bytes.len() < cursor + 2 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: missing base_path len",
        ));
    }
    let base_len = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]) as usize;
    cursor += 2;
    if bytes.len() < cursor + base_len {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: base_path truncated",
        ));
    }
    let base_path = std::str::from_utf8(&bytes[cursor..cursor + base_len])
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("base_path utf-8: {e}")))?
        .to_string();
    cursor += base_len;
    if bytes.len() < cursor + 2 {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: missing doc_root len",
        ));
    }
    let doc_len = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]) as usize;
    cursor += 2;
    if bytes.len() < cursor + doc_len {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "tunnel frame: doc_root truncated",
        ));
    }
    let doc_root = std::str::from_utf8(&bytes[cursor..cursor + doc_len])
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("doc_root utf-8: {e}")))?
        .to_string();
    cursor += doc_len;

    let target = match kind {
        FRAME_TARGET_HTTP => BackendTarget::Http {
            host,
            port,
            base_path,
        },
        FRAME_TARGET_HTTPS => BackendTarget::Https {
            host,
            port,
            base_path,
        },
        FRAME_TARGET_FILE => BackendTarget::File {
            doc_root: std::path::PathBuf::from(doc_root),
        },
        other => {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("unknown target kind 0x{other:02x}"),
            ));
        }
    };

    // `client_ip` was added after the initial wire format; the
    // old layout (no `client_ip` block) is still in the wild.
    // The new layout inserts a 1-byte flag (0/1) right before
    // the request bytes. We can detect which layout we got by
    // peeking at `bytes[cursor]`: the new flag is always 0 or 1,
    // while the first byte of an HTTP request method is always
    // an ASCII letter (G/P/C/H/D/L/...) which is all > 1. So
    // 0/1 is unambiguously the new flag; anything else is the
    // start of the request.
    let (client_ip, request) = match bytes.get(cursor).copied() {
        Some(0) | Some(1) => {
            // New format.
            cursor += 1;
            let ip = if bytes[cursor - 1] == 1 {
                if bytes.len() < cursor + 2 {
                    return Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        "tunnel frame: missing client_ip length",
                    ));
                }
                let ip_len = u16::from_be_bytes([bytes[cursor], bytes[cursor + 1]]) as usize;
                cursor += 2;
                if bytes.len() < cursor + ip_len {
                    return Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        "tunnel frame: client_ip truncated",
                    ));
                }
                let s = std::str::from_utf8(&bytes[cursor..cursor + ip_len])
                    .map_err(|e| {
                        Error::new(ErrorKind::InvalidData, format!("client_ip utf-8: {e}"))
                    })?
                    .to_string();
                cursor += ip_len;
                Some(s)
            } else {
                None
            };
            let req = crate::tunnel::parse_http_request_bytes(&bytes[cursor..])?;
            (ip, req)
        }
        _ => {
            // Old format: no client_ip block.
            let req = crate::tunnel::parse_http_request_bytes(&bytes[cursor..])?;
            (None, req)
        }
    };

    Ok(TunnelHttpFrame {
        request,
        target,
        host_mode,
        host_custom,
        is_upgrade,
        is_streaming,
        client_ip,
    })
}

fn host_mode_byte(mode: HostMode) -> u8 {
    match mode {
        HostMode::Passthrough => FRAME_HOST_PASSTHROUGH,
        HostMode::Backend => FRAME_HOST_BACKEND,
        HostMode::Custom => FRAME_HOST_CUSTOM,
    }
}

// ── is_streaming_request ──────────────────────────────────────

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
/// layer. The `is_streaming` flag in `TunnelHttpFrame` is what
/// ultimately decides the dispatch; this helper just sets it.
pub fn is_streaming_request(request: &HttpRequest) -> bool {
    request.headers.iter().any(|(k, v)| {
        (k.eq_ignore_ascii_case("Accept") || k.eq_ignore_ascii_case("Content-Type"))
            && v.to_ascii_lowercase().contains("text/event-stream")
    })
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
                client_ip: None,
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
            client_ip: None,
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
            client_ip: None,
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
            client_ip: None,
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
        // No client_ip → no X-Forwarded-For / X-Real-IP (we
        // can't make up an IP that isn't there).
        assert!(!req.headers.iter().any(|(k, _)| k == "X-Forwarded-For"));
        assert!(!req.headers.iter().any(|(k, _)| k == "X-Real-IP"));
    }

    /// `client_ip = Some(...)` triggers the X-Forwarded-For +
    /// X-Real-IP injection. The XFF chain is APPENDED (not
    /// replaced) so multi-hop setups preserve upstream proxy
    /// entries. This is the I-21 / I-22 behavior at the
    /// `apply_proxy_policy` level; the e2e layer (`tests/`)
    /// covers the wire-level flow.
    #[test]
    fn apply_proxy_policy_appends_client_ip_to_xff_chain() {
        // 1. Fresh request (no XFF yet) → policy starts the chain.
        let mut req = mk("GET", "/", &[("Host", "dev.yaitoo.cn")], &[]);
        let ctx = ProxyCtx {
            original_host: "dev.yaitoo.cn".into(),
            original_scheme: Scheme::Http,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            client_ip: Some("203.0.113.42".into()),
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
            "fresh request: chain starts with the client IP"
        );
        let xri = req
            .headers
            .iter()
            .find(|(k, _)| k == "X-Real-IP")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert_eq!(xri, "203.0.113.42", "X-Real-IP is the bare client IP");

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

        // 3. Duplicate (client_ip already at the chain tail) →
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
            client_ip: None,
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

    // I-7
    #[test]
    fn tunnel_frame_roundtrip() {
        for mode in [HostMode::Passthrough, HostMode::Backend, HostMode::Custom] {
            let custom = if mode == HostMode::Custom {
                Some("custom.example.com".into())
            } else {
                None
            };
            // Build a frame; `encode_http_request` will auto-add
            // a `Content-Length: 7` for the body, so after the
            // round-trip we expect the request to carry that
            // header. The test compares the **decoded** frame
            // against a frame that already has Content-Length.
            let mut request = mk(
                "POST",
                "http://10.0.0.1:9000/chat?x=1",
                &[
                    ("Host", "10.0.0.1:9000"),
                    ("Content-Type", "application/json"),
                ],
                b"{\"a\":1}",
            );
            request
                .headers
                .push(("Content-Length".to_string(), "7".to_string()));
            let target = BackendTarget::Http {
                host: "10.0.0.1".into(),
                port: 9000,
                base_path: String::new(),
            };
            let frame = TunnelHttpFrame {
                request,
                target,
                host_mode: mode,
                host_custom: custom.clone(),
                is_upgrade: true,
                is_streaming: false,
                client_ip: None,
            };
            let bytes = encode_tunnel_frame(&frame);
            let decoded = decode_tunnel_frame(&bytes).unwrap();
            assert_eq!(decoded, frame);
        }
        // Also test no-custom / non-upgrade, no body. The
        // encoder auto-adds `Content-Length: 0` for empty-body
        // requests, so build that into the expected frame.
        let mut request = mk("GET", "/", &[("Host", "x")], &[]);
        request
            .headers
            .push(("Content-Length".to_string(), "0".to_string()));
        let target = BackendTarget::Http {
            host: "127.0.0.1".into(),
            port: 80,
            base_path: String::new(),
        };
        let frame = TunnelHttpFrame {
            request,
            target,
            host_mode: HostMode::Passthrough,
            host_custom: None,
            is_upgrade: false,
            is_streaming: false,
            client_ip: None,
        };
        let bytes = encode_tunnel_frame(&frame);
        let decoded = decode_tunnel_frame(&bytes).unwrap();
        assert_eq!(decoded, frame);
    }

    // is_streaming_request: heuristics for SSE / streaming responses.
    // Used by ngx to flip `TunnelHttpFrame::is_streaming = true`,
    // which routes the request to the byte-relay path instead of
    // the buffering HttpResponse path.
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
