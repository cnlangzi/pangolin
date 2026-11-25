//! The client side of the Server-Sent Events wire format.
//!
//! The dashboard pushes commands down an SSE subscription (issue
//! #262), so the node has to read the format: lines of `field: value`,
//! frames separated by a blank line, comments that start with a colon.
//!
//! The parser is **incremental**: the bytes arrive in whatever chunks
//! the network hands over, and a chunk may end in the middle of a
//! line — or in the middle of a `\r\n`. The parser therefore keeps the
//! remainder of the stream in a buffer and only consumes whole lines.
//!
//! The rules are the ones of the WHATWG event stream format:
//!
//! * a line that starts with `:` is a **comment** and is ignored;
//! * a line is `field: value` — one leading space of the value is
//!   dropped; a line without a colon is a field with an empty value;
//! * `data`, `event`, `id` and `retry` are the known fields; every
//!   other field is ignored;
//! * a **blank line** dispatches the frame: the `data` lines are
//!   joined with `\n`;
//! * a frame without any `data` is **not** dispatched at all.

/// One frame of an SSE stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseFrame {
    /// The `event:` field — the frame's type.
    pub event: Option<String>,
    /// The `data:` field — the lines joined with `\n`.
    pub data: String,
    /// The `id:` field — the stream's last event id.
    pub id: Option<String>,
    /// The `retry:` field — the reconnection time in milliseconds.
    pub retry: Option<u64>,
}

/// The incremental parser of an SSE stream.
#[derive(Debug, Default)]
pub struct SseParser {
    /// The bytes of the stream that did not form a whole line yet.
    buffer: Vec<u8>,
    /// The `data:` lines of the frame that is being assembled.
    data: Vec<String>,
    /// The `event:` field of the frame that is being assembled.
    event: Option<String>,
    /// The last `id:` the stream carried.
    id: Option<String>,
    /// The last `retry:` the stream carried.
    retry: Option<u64>,
}

impl SseParser {
    /// An empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of the stream; the frames it completed are
    /// returned.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(line) = self.take_line(false) {
            if let Some(frame) = self.handle_line(&line) {
                frames.push(frame);
            }
        }
        frames
    }

    /// The end of the stream: the bytes that are left form the last
    /// line, and a frame that is still being assembled is dispatched.
    ///
    /// The end of the stream dispatches what is pending — the
    /// dashboard may close the connection right after a frame, without
    /// the blank line that would have closed it.
    pub fn finish(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        while let Some(line) = self.take_line(true) {
            if let Some(frame) = self.handle_line(&line) {
                frames.push(frame);
            }
        }
        if let Some(frame) = self.dispatch() {
            frames.push(frame);
        }
        frames
    }

    /// Take the next whole line out of the buffer.
    ///
    /// `at_eof` decides what a trailing `\r` means: at the end of the
    /// stream it terminates the line, in the middle of the stream it
    /// may be the first half of a `\r\n` — so the line waits for the
    /// next chunk.
    fn take_line(&mut self, at_eof: bool) -> Option<String> {
        let mut i = 0;
        while i < self.buffer.len() {
            match self.buffer[i] {
                b'\n' => {
                    let line = self.take_bytes(i);
                    // The terminator itself is consumed — a line that
                    // is not consumed would be read again, forever.
                    self.buffer.drain(..1);
                    return Some(line);
                }
                b'\r' => {
                    if i + 1 == self.buffer.len() {
                        if at_eof {
                            let line = self.take_bytes(i);
                            self.buffer.drain(..1);
                            return Some(line);
                        }
                        // The `\n` of a `\r\n` may still be coming.
                        return None;
                    }
                    let skip = if self.buffer[i + 1] == b'\n' { 2 } else { 1 };
                    let line = self.take_bytes(i);
                    self.buffer.drain(..skip);
                    return Some(line);
                }
                _ => i += 1,
            }
        }
        if at_eof && !self.buffer.is_empty() {
            let line = self.take_bytes(self.buffer.len());
            return Some(line);
        }
        None
    }

    /// Take the first `len` bytes of the buffer as a line.
    fn take_bytes(&mut self, len: usize) -> String {
        let line: Vec<u8> = self.buffer.drain(..len).collect();
        String::from_utf8_lossy(&line).into_owned()
    }

    /// Handle one line; returns the frame it completed, if any.
    fn handle_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.dispatch();
        }
        // A comment — the dashboard uses them as keep-alives.
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            "id" => {
                // The spec drops an id that carries a NUL.
                if !value.contains('\0') {
                    self.id = Some(value.to_string());
                }
            }
            // A `retry` that is not a number is ignored, like every
            // other field the parser does not know.
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                self.retry = value.parse().ok();
            }
            // Every other field is ignored.
            _ => {}
        }
        None
    }

    /// Dispatch the frame that was being assembled.
    fn dispatch(&mut self) -> Option<SseFrame> {
        if self.data.is_empty() {
            // A frame without data is not dispatched — and it resets
            // the event type.
            self.event = None;
            return None;
        }
        let frame = SseFrame {
            event: self.event.take(),
            data: self.data.join("\n"),
            id: self.id.clone(),
            retry: self.retry,
        };
        self.data.clear();
        self.event = None;
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_data_frame_is_parsed() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"data: hello\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
        assert_eq!(frames[0].event, None);
    }

    #[test]
    fn the_event_field_is_read() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"event: command\ndata: {\"id\":\"c-1\"}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("command"));
        assert_eq!(frames[0].data, "{\"id\":\"c-1\"}");
    }

    #[test]
    fn several_data_lines_are_joined_with_a_newline() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"data: one\ndata: two\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "one\ntwo");
    }

    #[test]
    fn a_comment_is_ignored() {
        let mut parser = SseParser::new();
        let frames = parser.push(b": keep-alive\ndata: hello\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn a_frame_without_data_is_not_dispatched() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"event: ping\n\ndata: hello\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
        // The event type of the frame that was dropped does not leak
        // into the next one.
        assert_eq!(frames[0].event, None);
    }

    #[test]
    fn the_id_and_the_retry_are_read() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"id: 42\nretry: 1500\ndata: hello\n\n");
        assert_eq!(frames[0].id.as_deref(), Some("42"));
        assert_eq!(frames[0].retry, Some(1500));
    }

    #[test]
    fn a_frame_split_across_chunks_is_still_parsed() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: hel").is_empty());
        assert!(parser.push(b"lo").is_empty());
        let frames = parser.push(b"\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn a_crlf_split_across_chunks_is_still_parsed() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: hello\r").is_empty());
        let frames = parser.push(b"\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn several_frames_in_one_chunk_are_parsed() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "one");
        assert_eq!(frames[1].data, "two");
    }

    #[test]
    fn a_field_without_a_colon_is_a_field_with_an_empty_value() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"data\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "");
    }

    #[test]
    fn only_one_leading_space_of_the_value_is_dropped() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"data:  hello\n\n");
        assert_eq!(frames[0].data, " hello");
    }

    #[test]
    fn the_last_line_of_the_stream_is_read_at_the_end() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: hello").is_empty());
        let frames = parser.finish();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn an_unknown_field_is_ignored() {
        let mut parser = SseParser::new();
        let frames = parser.push(b"whatever: value\ndata: hello\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }
}
