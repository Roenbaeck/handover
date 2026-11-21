//! Incremental Server-Sent Events parsing that keeps each event's original bytes.

use bytes::Bytes;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    /// Joined `data:` lines.
    pub data: String,
    /// The event exactly as received, including the terminating blank line.
    pub raw: Bytes,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every event completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep)) = find_boundary(&self.buf) {
            let raw: Vec<u8> = self.buf.drain(..end + sep).collect();
            let text = String::from_utf8_lossy(&raw[..end]).into_owned();
            let mut event = None;
            let mut data: Vec<&str> = Vec::new();
            for line in text.split('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line);
                if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v));
                } else if let Some(v) = line.strip_prefix("event:") {
                    event = Some(v.trim().to_string());
                }
            }
            if data.is_empty() && event.is_none() {
                continue; // comment or keep-alive
            }
            out.push(SseEvent { event, data: data.join("\n"), raw: Bytes::from(raw) });
        }
        out
    }

    /// Bytes received after the last complete event (a truncated stream).
    pub fn remainder(&self) -> &[u8] {
        &self.buf
    }
}

fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

/// `data: <json>\n\n`
pub fn data(json: &serde_json::Value) -> Bytes {
    Bytes::from(format!("data: {json}\n\n"))
}

/// `event: <name>\ndata: <json>\n\n` (Anthropic style).
pub fn named(event: &str, json: &serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {json}\n\n"))
}

pub fn done() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_across_chunks_and_keeps_raw() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"a\"").is_empty());
        let ev = p.push(b":1}\n\n: keepalive\n\nevent: x\r\ndata: 2\r\n\r\ndata: [DO");
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].data, "{\"a\":1}");
        assert_eq!(&ev[0].raw[..], b"data: {\"a\":1}\n\n");
        assert_eq!(ev[1].event.as_deref(), Some("x"));
        assert_eq!(ev[1].data, "2");
        assert_eq!(p.remainder(), b"data: [DO");
    }
}
