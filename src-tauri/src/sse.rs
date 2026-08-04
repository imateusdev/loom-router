//! SSE framing/parsing helpers shared by the stream translators.

/// Incremental parser for upstream SSE streams (`data: ...` frames).
pub struct SseParser {
    buf: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Feed raw bytes; returns every complete SSE event seen so far.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut events = Vec::new();
        loop {
            // Frames are separated by a blank line (\n\n or \r\n\r\n).
            let sep = self
                .buf
                .find("\n\n")
                .map(|i| (i, 2))
                .or_else(|| self.buf.find("\r\n\r\n").map(|i| (i, 4)));
            let Some((idx, len)) = sep else { break };
            let frame: String = self.buf.drain(..idx + len).collect();
            if let Some(ev) = parse_frame(&frame) {
                events.push(ev);
            }
        }
        events
    }
}

fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

/// Build one SSE frame with explicit event name (Responses API style).
pub fn frame_with_event(event: &str, data: &serde_json::Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

/// Build one SSE frame with only data (Chat Completions style).
pub fn frame_data(data: &serde_json::Value) -> Vec<u8> {
    format!("data: {data}\n\n").into_bytes()
}

pub fn frame_done() -> Vec<u8> {
    b"data: [DONE]\n\n".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_frames() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"a\":1}").is_empty());
        let out = p.push(b"\n\ndata: {\"b\":2}\n\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].data, "{\"a\":1}");
        assert_eq!(out[1].data, "{\"b\":2}");
    }

    #[test]
    fn captures_event_name() {
        let mut p = SseParser::new();
        let out = p.push(b"event: response.output_text.delta\ndata: {}\n\n");
        assert_eq!(out[0].event.as_deref(), Some("response.output_text.delta"));
    }
}
