//! In-memory event buffer for admin dashboard activity feed.
//!
//! Maintains a bounded ring buffer of recent events (max 100) in memory.
//! Events are not persisted — they disappear on restart.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Access log entry for real-time monitoring via SSE.
///
/// Constructed by `ngx`'s `response_filter` on every proxied request
/// (issue #73). The struct is deliberately small + `Clone` — every
/// push goes through a `tokio::sync::broadcast` channel, so we want
/// to copy the bytes, not borrow them. The `backend` field is a
/// pre-formatted string ("tun:office", "direct:1.2.3.4:8080",
/// "file://…") derived from the existing `site.backend` so the
/// dashboard renders a human-readable value without re-running the
/// backend parser on every read.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessLogEntry {
    pub timestamp: DateTime<Utc>,
    pub method: String,
    pub path: String,
    pub host: String,
    pub status: u16,
    pub duration_ms: u64,
    pub backend: String,
    pub client_ip: String,
}

/// Bounded ring buffer of recent access log entries.
///
/// Uses `parking_lot::Mutex` (not `std::sync::Mutex`) because:
///   - The lock is held for microseconds (one `push_back` + one
///     bound check); parking_lot's fast-path acquire is the right
///     tool.
///   - `parking_lot::Mutex` is not poisonable, so a panic inside
///     the locked region never surfaces as a `.lock().unwrap()`
///     abort on the next push — exactly the right semantics for a
///     best-effort log buffer that must never crash a request.
///
/// Sized at construction time via `new(capacity)`. Capacity of 0
/// is allowed and turns the buffer into a no-op (push is a
/// single integer compare; no allocation). This matches the
/// `ngx.yml` default for `log.access_log_recent: 100` but lets a
/// user disable buffering entirely if they want to keep only the
/// live broadcast.
pub struct AccessLogBuffer {
    entries: parking_lot::Mutex<VecDeque<AccessLogEntry>>,
    capacity: usize,
}

impl AccessLogBuffer {
    /// Create a new ring buffer with the given capacity. A capacity
    /// of 0 means "drop every entry on push" — useful as a
    /// disable-buffer knob without changing call sites.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.min(MAX_EVENTS.saturating_mul(1024)); // sanity cap
        Self {
            entries: parking_lot::Mutex::new(VecDeque::with_capacity(cap)),
            capacity: cap,
        }
    }

    /// Push an entry to the back of the buffer, evicting the oldest
    /// entry if capacity is exceeded. Never blocks (the buffer is
    /// sync) and never panics on capacity=0 (the loop is a single
    /// integer compare).
    pub fn push(&self, entry: AccessLogEntry) {
        if self.capacity == 0 {
            return;
        }
        let mut entries = self.entries.lock();
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Snapshot the buffer in chronological order (oldest first).
    ///
    /// Used by the SSE endpoint to replay the ring on connect. The
    /// caller (an `async fn` serving a streaming response) takes
    /// the lock once, clones the entries into a local `Vec`, and
    /// drops the lock before the first `await` — so the lock is
    /// never held across a network read.
    pub fn snapshot(&self) -> Vec<AccessLogEntry> {
        let entries = self.entries.lock();
        entries.iter().cloned().collect()
    }

    /// Read the current capacity. Exposed so the SSE endpoint can
    /// decide whether to send a `replay` hint without consulting
    /// `App.config` separately.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Maximum number of events to retain in the buffer.
pub const MAX_EVENTS: usize = 100;

/// Event types for the dashboard activity feed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventType {
    /// Tunnel node connected.
    TunConnected { name: String },
    /// Tunnel node disconnected.
    TunDisconnected { name: String },
    /// Certificate renewed successfully.
    CertRenewed { domain: String },
    /// Certificate renewal failed.
    CertRenewFailed { domain: String, error: String },
    /// Site configuration updated.
    SiteUpdated { name: String },
    /// Domain created or updated.
    DomainUpdated { domain: String, site: String },
    /// Generic info message.
    Info { message: String },
    /// Certificate was issued for the first time (or after manual delete).
    CertIssued { domain: String },
    /// Auto-issuance was skipped for a domain (e.g. wildcard without DNS).
    CertIssuanceSkipped { domain: String, reason: String },
}

/// A single event with timestamp.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub event: EventType,
}

impl Event {
    /// Create a new event with the current timestamp.
    pub fn new(event: EventType) -> Self {
        Self {
            timestamp: Utc::now(),
            event,
        }
    }
}

/// Thread-safe in-memory event buffer.
#[derive(Default)]
pub struct EventBuffer {
    /// Bounded ring buffer of recent events.
    events: std::sync::Mutex<VecDeque<Event>>,
}

impl EventBuffer {
    /// Create a new empty event buffer.
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(VecDeque::with_capacity(MAX_EVENTS)),
        }
    }

    /// Add an event to the buffer.
    /// Removes oldest events when capacity is exceeded.
    pub fn push(&self, event: Event) {
        let mut events = self.events.lock().unwrap();
        if events.len() >= MAX_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// Get all events in reverse chronological order (newest first).
    pub fn get_all(&self) -> Vec<Event> {
        let events = self.events.lock().unwrap();
        events.iter().rev().cloned().collect()
    }

    /// Get the most recent N events.
    pub fn get_recent(&self, n: usize) -> Vec<Event> {
        let events = self.events.lock().unwrap();
        events.iter().rev().take(n).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_buffer_basic_push_get() {
        let buffer = EventBuffer::new();
        buffer.push(Event::new(EventType::TunConnected {
            name: "office".into(),
        }));
        buffer.push(Event::new(EventType::TunDisconnected {
            name: "home".into(),
        }));

        let events = buffer.get_all();
        assert_eq!(events.len(), 2);
        // Most recent first
        match &events[0].event {
            EventType::TunDisconnected { name } => assert_eq!(name, "home"),
            _ => panic!("expected TunDisconnected"),
        }
        match &events[1].event {
            EventType::TunConnected { name } => assert_eq!(name, "office"),
            _ => panic!("expected TunConnected"),
        }
    }

    #[test]
    fn event_buffer_capacity_exceeded() {
        let buffer = EventBuffer::new();
        // Add more than MAX_EVENTS events
        for i in 0..(MAX_EVENTS + 10) {
            buffer.push(Event::new(EventType::Info {
                message: format!("event-{}", i),
            }));
        }

        let events = buffer.get_all();
        assert_eq!(events.len(), MAX_EVENTS);
        // First event should be the oldest that survived (event-10)
        match &events[MAX_EVENTS - 1].event {
            EventType::Info { message } => assert!(message.contains("event-10")),
            _ => panic!("expected Info event"),
        }
    }

    #[test]
    fn event_buffer_get_recent() {
        let buffer = EventBuffer::new();
        for i in 0..10 {
            buffer.push(Event::new(EventType::Info {
                message: format!("event-{}", i),
            }));
        }

        let recent = buffer.get_recent(3);
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn event_serialization() {
        let event = Event::new(EventType::TunConnected {
            name: "office".into(),
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"TunConnected\""));
        assert!(json.contains("\"name\":\"office\""));
    }

    #[test]
    fn access_log_entry_serialization() {
        let entry = AccessLogEntry {
            timestamp: Utc::now(),
            method: "GET".into(),
            path: "/test".into(),
            host: "example.com".into(),
            status: 200,
            duration_ms: 42,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "192.168.1.1".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: AccessLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/test");
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.duration_ms, 42);
    }

    fn make_entry(method: &str, path: &str) -> AccessLogEntry {
        AccessLogEntry {
            timestamp: Utc::now(),
            method: method.into(),
            path: path.into(),
            host: "example.com".into(),
            status: 200,
            duration_ms: 1,
            backend: "direct:127.0.0.1:8080".into(),
            client_ip: "192.168.1.1".into(),
        }
    }

    #[test]
    fn access_log_buffer_capacity_zero_is_noop() {
        // capacity=0 means "disable ring buffer entirely" — every
        // push is silently dropped, snapshot is empty.
        let buf = AccessLogBuffer::new(0);
        buf.push(make_entry("GET", "/a"));
        buf.push(make_entry("GET", "/b"));
        assert!(buf.snapshot().is_empty());
        assert_eq!(buf.capacity(), 0);
    }

    #[test]
    fn access_log_buffer_evicts_oldest_on_overflow() {
        let buf = AccessLogBuffer::new(3);
        for i in 0..5 {
            buf.push(make_entry("GET", &format!("/p{i}")));
        }
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 3);
        // snapshot() returns in chronological (insertion) order.
        // Entries /p0 and /p1 were evicted; the surviving entries
        // are /p2, /p3, /p4 in that order.
        assert_eq!(snap[0].path, "/p2");
        assert_eq!(snap[1].path, "/p3");
        assert_eq!(snap[2].path, "/p4");
    }

    #[test]
    fn access_log_buffer_snapshot_chronological_order() {
        // snapshot() returns oldest-first so an SSE endpoint can
        // replay them as it received them.
        let buf = AccessLogBuffer::new(10);
        buf.push(make_entry("GET", "/a"));
        buf.push(make_entry("GET", "/b"));
        buf.push(make_entry("GET", "/c"));
        let snap = buf.snapshot();
        assert_eq!(
            snap.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["/a", "/b", "/c"]
        );
    }
}
