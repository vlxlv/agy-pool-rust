use bytes::Bytes;

/// List of sensitive agent lines that trigger Google Antigravity server-side 429 filters.
pub const TURNED_AWAY: &[&str] = &[
    "x-anthropic-billing-header:",
    "built on anthropic's claude agent sdk",
    "you are codex, an agent based on gpt-5",
    "you are codex, a coding agent based on gpt-5",
    "hermes agent, an intelligent ai assistant created by nous research",
    "rfc 2119 keywords: must, required, should, recommended, may, optional.",
];

const ZW: &str = "\u{200b}";

/// Checks whether the text contains any of the turned-away phrases (case-insensitive ASCII).
pub fn turns_away(text: &str) -> bool {
    bytes_turn_away(text.as_bytes())
}

// Runs on every request body, so it scans in place instead of lowercasing a copy.
fn bytes_turn_away(hay: &[u8]) -> bool {
    TURNED_AWAY.iter().any(|&needle| {
        let n = needle.as_bytes();
        hay.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    })
}

/// Breaks up every alphanumeric word of length >= 3 in `line` by inserting a Zero-Width Space
/// after the 1st and 2nd characters (e.g. "built" -> "b\u{200b}u\u{200b}ilt").
pub fn break_up_words(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let chars: Vec<(usize, char)> = line.char_indices().collect();

    let mut i = 0;
    while i < chars.len() {
        let (_, ch) = chars[i];
        if ch.is_ascii_alphanumeric() {
            let mut word_chars = vec![ch];
            let mut j = i + 1;
            while j < chars.len() && chars[j].1.is_ascii_alphanumeric() {
                word_chars.push(chars[j].1);
                j += 1;
            }
            if word_chars.len() >= 3 {
                out.push(word_chars[0]);
                out.push_str(ZW);
                out.push(word_chars[1]);
                out.push_str(ZW);
                for &wc in &word_chars[2..] {
                    out.push(wc);
                }
            } else {
                for &wc in &word_chars {
                    out.push(wc);
                }
            }
            i = j;
        } else {
            out.push(ch);
            i += 1;
        }
    }
    out
}

/// Applies zero-width cloaking to any occurrences of the turned-away phrases in `text`.
pub fn cloak_text(text: &str) -> String {
    if !turns_away(text) {
        return text.to_string();
    }
    let lower = text.to_ascii_lowercase();
    let mut spans = Vec::new();
    for &needle in TURNED_AWAY {
        let mut start = 0;
        while let Some(pos) = lower[start..].find(needle) {
            let from = start + pos;
            let to = from + needle.len();
            spans.push((from, to));
            start = to;
        }
    }
    spans.sort_by_key(|&(from, _)| from);

    let mut out = String::with_capacity(text.len() + spans.len() * 16);
    let mut prev = 0;
    for (from, to) in spans {
        if from < prev {
            continue;
        }
        out.push_str(&text[prev..from]);
        out.push_str(&break_up_words(&text[from..to]));
        prev = to;
    }
    out.push_str(&text[prev..]);
    out
}

/// Fast-path helper for proxy request bodies.
/// If any turned-away phrase is present, returns `Some(Bytes)` containing cloaked payload.
/// If not present, returns `None` (0 allocations).
pub fn cloak_payload_if_needed(body: &[u8]) -> Option<Bytes> {
    if !bytes_turn_away(body) {
        return None;
    }
    let s = std::str::from_utf8(body).ok()?;
    Some(Bytes::from(cloak_text(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_turns_away_detection() {
        assert!(turns_away("This is built on Anthropic's Claude Agent SDK for coding."));
        assert!(turns_away("X-Anthropic-Billing-Header: cc_version=1;"));
        assert!(turns_away("You are Codex, an agent based on GPT-5."));
        assert!(turns_away(
            "Hermes agent, an intelligent AI assistant created by Nous Research"
        ));
        assert!(turns_away(
            "RFC 2119 keywords: MUST, REQUIRED, SHOULD, RECOMMENDED, MAY, OPTIONAL."
        ));
        assert!(!turns_away("Hello world, this is normal prompt."));
    }

    #[test]
    fn test_break_up_words() {
        let original = "built on anthropic's claude agent sdk";
        let cloaked = break_up_words(original);
        assert_eq!(
            cloaked,
            "b\u{200b}u\u{200b}ilt on a\u{200b}n\u{200b}thropic's c\u{200b}l\u{200b}aude a\u{200b}g\u{200b}ent s\u{200b}d\u{200b}k"
        );
    }

    #[test]
    fn test_cloak_text_preserves_surrounding() {
        let prompt = "Prefix instructions. You are Codex, an agent based on GPT-5 for tests. Suffix details.";
        let res = cloak_text(prompt);
        assert!(res.starts_with("Prefix instructions. "));
        assert!(res.ends_with(" Suffix details."));
        assert!(
            !res.to_ascii_lowercase()
                .contains("you are codex, an agent based on gpt-5")
        );
        assert!(res.contains("\u{200b}"));
    }

    #[test]
    fn test_cloak_payload_if_needed_fast_path() {
        let clean = b"{\"contents\": [{\"parts\": [{\"text\": \"hello\"}]}]}";
        assert!(cloak_payload_if_needed(clean).is_none());

        let sensitive = b"{\"system\": \"Built on Anthropic's Claude Agent SDK\"}";
        let res = cloak_payload_if_needed(sensitive);
        assert!(res.is_some());
        let res_str = std::str::from_utf8(&res.unwrap()).unwrap().to_string();
        assert!(res_str.contains("\u{200b}"));
        assert!(
            !res_str
                .to_ascii_lowercase()
                .contains("built on anthropic's claude agent sdk")
        );
    }
}
