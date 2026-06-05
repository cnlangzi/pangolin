//! In-memory event buffer for admin dashboard activity feed.
//!
//! Maintains a bounded ring buffer of recent events (max 100) in memory.
//! Events are not persisted — they disappear on restart.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

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

    /// Get events newer than a given timestamp (for incremental polling).
    pub fn get_since(&self, since: DateTime<Utc>) -> Vec<Event> {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .rev()
            .filter(|e| e.timestamp > since)
            .cloned()
            .collect()
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
        buffer.push(Event::new(EventType::TunConnected { name: "office".into() }));
        buffer.push(Event::new(EventType::TunDisconnected { name: "home".into() }));

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
            buffer.push(Event::new(EventType::Info { message: format!("event-{}", i) }));
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
            buffer.push(Event::new(EventType::Info { message: format!("event-{}", i) }));
        }

        let recent = buffer.get_recent(3);
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn event_serialization() {
        let event = Event::new(EventType::TunConnected { name: "office".into() });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"TunConnected\""));
        assert!(json.contains("\"name\":\"office\""));
    }
}