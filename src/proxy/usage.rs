use sha2::{Digest, Sha256};
use std::io::Read;

/// UsageTap tracks prompt and cached token counts from responses as they stream.
pub struct UsageTap<R> {
    inner: R,
    carry: Vec<u8>,
    pub prompt: i64,
    pub cached: i64,
}

impl<R> UsageTap<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            carry: Vec::new(),
            prompt: 0,
            cached: -1,
        }
    }

    pub fn prompt_tokens(&self) -> i64 {
        self.prompt
    }

    pub fn cached_tokens(&self) -> Option<i64> {
        if self.cached >= 0 { Some(self.cached) } else { None }
    }

    pub fn scan(&mut self, b: &[u8]) {
        let mut buf = Vec::with_capacity(self.carry.len() + b.len());
        buf.extend_from_slice(&self.carry);
        buf.extend_from_slice(b);

        let s = String::from_utf8_lossy(&buf);
        let keys = ["promptTokenCount", "cachedContentTokenCount"];
        for key in keys {
            let pattern = format!("\"{key}\"");
            let mut start = 0;
            while let Some(pos) = s[start..].find(&pattern) {
                let idx = start + pos + pattern.len();
                let rem = s[idx..].trim_start();
                if let Some(after_colon) = rem.strip_prefix(':') {
                    let num_str = after_colon.trim_start();
                    let digits_end = num_str.find(|c: char| !c.is_ascii_digit()).unwrap_or(num_str.len());
                    if digits_end > 0
                        && let Ok(v) = num_str[..digits_end].parse::<i64>()
                    {
                        if key == "promptTokenCount" {
                            self.prompt = v;
                        } else {
                            self.cached = v;
                        }
                    }
                }
                start = idx;
            }
        }

        let tail_start = buf.len().saturating_sub(64);
        self.carry = buf[tail_start..].to_vec();
    }
}

impl<R: Read> Read for UsageTap<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.scan(&buf[..n]);
        }
        Ok(n)
    }
}

/// session_tag returns a short, stable, non-reversible label for a session key in logs.
pub fn session_tag(session_key: &str) -> String {
    if session_key.is_empty() {
        return "-".to_string();
    }
    let sum = Sha256::digest(session_key.as_bytes());
    hex::encode(&sum[..4])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    struct OneByteReader<R> {
        inner: R,
    }

    impl<R: Read> Read for OneByteReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            self.inner.read(&mut buf[..1])
        }
    }

    #[test]
    fn usage_tap_split_across_reads() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let body = concat!(
            "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\": 10}}}\n\n",
            "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\": 12345,\"cachedContentTokenCount\": 9876}}}\n\n"
        );
        let reader = OneByteReader { inner: body.as_bytes() };
        let mut tap = UsageTap::new(reader);
        let mut out = String::new();
        tap.read_to_string(&mut out).unwrap();
        assert_eq!(out, body);
        assert_eq!(tap.prompt, 12345);
        assert_eq!(tap.cached, 9876);
        assert_eq!(tap.prompt_tokens(), 12345);
        assert_eq!(tap.cached_tokens(), Some(9876));
    }

    #[test]
    fn usage_tap_cached_not_reported() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut tap = UsageTap::new(&b""[..]);
        tap.scan(br#"{"usageMetadata":{"promptTokenCount": 42}}"#);
        assert_eq!(tap.prompt, 42);
        assert_eq!(tap.cached, -1);
        assert_eq!(tap.cached_tokens(), None);
    }

    #[test]
    fn session_tag_output() {
        assert_eq!(session_tag(""), "-");
        assert_eq!(session_tag("session-1").len(), 8);
    }
}
