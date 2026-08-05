//! SSE framing/parsing helpers shared by the stream translators.

/// Safety cap for one unterminated frame: a well-formed SSE stream emits
/// a blank line per frame, so a buffer larger than this means a stuck or
/// malicious upstream. Beyond the cap we drop the partial frame instead
/// of buffering without bound.
const MAX_BUFFER: usize = 1024 * 1024; // 1 MiB

/// Incremental parser for upstream SSE streams (`data: ...` frames).
pub struct SseParser {
    /// Undecoded raw bytes — holds the tail of a UTF-8 multibyte sequence
    /// split across chunk boundaries (at most 3 bytes in practice, since
    /// every push decodes everything decodable). Never lossy: incomplete
    /// sequences are carried over to the next chunk, so accents/CJK
    /// survive chunk splits intact.
    raw: Vec<u8>,
    /// Decoded text not yet terminated by a blank-line frame separator.
    text: String,
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
        Self {
            raw: Vec::new(),
            text: String::new(),
        }
    }

    /// Feed raw bytes; returns every complete SSE event seen so far.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.raw.extend_from_slice(bytes);
        self.decode_available();
        self.take_frames()
    }

    /// Decode as much of `raw` as forms valid UTF-8, keeping any
    /// incomplete trailing multibyte sequence for the next chunk.
    fn decode_available(&mut self) {
        loop {
            match std::str::from_utf8(&self.raw) {
                Ok(s) => {
                    self.text.push_str(s);
                    self.raw.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // valid_up_to guarantees this prefix is well-formed.
                        let prefix = String::from_utf8_lossy(&self.raw[..valid]).into_owned();
                        self.text.push_str(&prefix);
                    }
                    match e.error_len() {
                        // Genuinely invalid byte(s): replace and continue
                        // decoding after them.
                        Some(bad) => {
                            self.text.push('\u{FFFD}');
                            self.raw.drain(..valid + bad);
                        }
                        // Incomplete multibyte sequence at the end of the
                        // chunk: carry the bytes over to the next push.
                        None => {
                            self.raw.drain(..valid);
                            break;
                        }
                    }
                }
            }
        }
    }

    fn take_frames(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        loop {
            // Frames are separated by a blank line (\n\n or \r\n\r\n).
            let sep = self
                .text
                .find("\n\n")
                .map(|i| (i, 2))
                .or_else(|| self.text.find("\r\n\r\n").map(|i| (i, 4)));
            let Some((idx, len)) = sep else { break };
            let frame: String = self.text.drain(..idx + len).collect();
            if let Some(ev) = parse_frame(&frame) {
                events.push(ev);
            }
        }
        if self.text.len() > MAX_BUFFER {
            // No frame separator within 1 MiB: the upstream never
            // terminates frames (stuck or malicious). Drop the partial
            // frame so the buffer can't grow without bound; parsing
            // resumes cleanly at the next push.
            self.text.clear();
        }
        events
    }

    /// Signal end-of-stream: decode any leftover bytes (lossy here — an
    /// incomplete trailing sequence is unrecoverable at this point) and
    /// emit a final event if the buffer holds an unterminated frame.
    pub fn flush(&mut self) -> Vec<SseEvent> {
        if !self.raw.is_empty() {
            let tail = String::from_utf8_lossy(&self.raw).into_owned();
            self.text.push_str(&tail);
            self.raw.clear();
        }
        let frame = std::mem::take(&mut self.text);
        match parse_frame(&frame) {
            Some(ev) => vec![ev],
            None => Vec::new(),
        }
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

    #[test]
    fn multibyte_split_across_chunks_survives() {
        let mut p = SseParser::new();
        let payload = "data: {\"t\":\"café — 日本語\"}\n\n";
        // Feed one byte at a time: every multibyte sequence is split.
        let mut events = Vec::new();
        for b in payload.as_bytes().chunks(1) {
            events.extend(p.push(b));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"t\":\"café — 日本語\"}");
    }

    #[test]
    fn flush_decodes_trailing_bytes_lossy() {
        let mut p = SseParser::new();
        // Incomplete UTF-8 (\xC3 starts a 2-byte sequence) and no frame
        // terminator: nothing is emitted on push.
        assert!(p.push(b"data: caf\xC3").is_empty());
        let out = p.flush();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "caf\u{FFFD}");
        // Parser is empty afterwards.
        assert!(p.flush().is_empty());
    }

    #[test]
    fn unterminated_oversized_frame_is_dropped() {
        let mut p = SseParser::new();
        let big = vec![b'a'; MAX_BUFFER + 1];
        assert!(p.push(&big).is_empty());
        // Buffer was capped: a following well-formed frame still parses.
        let out = p.push(b"data: ok\n\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "ok");
    }
}
