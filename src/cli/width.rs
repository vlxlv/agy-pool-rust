pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut in_esc = false;
    let mut i = 0;
    while i < bytes.len() {
        if !in_esc && bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            in_esc = true;
            i += 2;
            continue;
        }
        if in_esc {
            if bytes[i].is_ascii_alphabetic() {
                in_esc = false;
            }
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub fn rune_width(r: char) -> usize {
    let u = r as u32;
    if u < 32 || (0x7f..0xa0).contains(&u) {
        return 0;
    }
    // Zero-width characters (zero-width spaces, joiners, BOM, soft hyphen)
    if u == 0x200B || u == 0x200C || u == 0x200D || u == 0x2060 || u == 0xFEFF || u == 0x00AD {
        return 0;
    }
    // Combining diacritical marks
    if (0x0300..=0x036F).contains(&u)
        || (0x1DC0..=0x1DFF).contains(&u)
        || (0x20D0..=0x20FF).contains(&u)
        || (0xFE20..=0xFE2F).contains(&u)
    {
        return 0;
    }
    // East Asian Wide / Fullwidth characters
    if (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0x9FFF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE10..=0xFE19).contains(&u)
        || (0xFE30..=0xFE6F).contains(&u)
        || (0xFF01..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x20000..=0x2FA1F).contains(&u)
        || (0x30000..=0x3134F).contains(&u)
    {
        return 2;
    }
    1
}

pub fn visible_width(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    let mut width = 0;
    let mut in_esc = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !in_esc && bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            in_esc = true;
            i += 2;
            continue;
        }
        if in_esc {
            if bytes[i].is_ascii_alphabetic() {
                in_esc = false;
            }
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        width += rune_width(ch);
        i += ch.len_utf8();
    }
    width
}

pub fn truncate_visible(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let total_visible = visible_width(s);
    if total_visible <= max_width {
        return s.to_string();
    }

    let budget = max_width.saturating_sub(1);

    let mut out = String::new();
    let mut cur_width = 0;
    let mut in_esc = false;
    let mut had_ansi = false;
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if !in_esc && bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            in_esc = true;
            had_ansi = true;
            out.push_str(&s[i..i + 2]);
            i += 2;
            continue;
        }
        if in_esc {
            out.push(bytes[i] as char);
            if bytes[i].is_ascii_alphabetic() {
                in_esc = false;
            }
            i += 1;
            continue;
        }

        let ch = s[i..].chars().next().unwrap();
        let rw = rune_width(ch);
        if cur_width + rw > budget {
            break;
        }
        out.push(ch);
        cur_width += rw;
        i += ch.len_utf8();
    }

    out.push('…');
    if had_ansi {
        out.push_str(crate::cli::CLR_RESET);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rune_width() {
        let cases = [
            ('a', 1),
            ('Z', 1),
            ('1', 1),
            ('-', 1),
            (' ', 1),
            ('中', 2),
            ('文', 2),
            ('日', 2),
            ('本', 2),
            ('語', 2),
            ('한', 2),
            ('글', 2),
            ('，', 2),
            ('！', 2),
            ('【', 2),
            ('\u{200B}', 0),
            ('\0', 0),
            ('\x1b', 0),
        ];

        for (r, want) in cases {
            let got = rune_width(r);
            assert_eq!(got, want, "rune_width({r}) = {got}, want {want}");
        }
    }

    #[test]
    fn test_visible_width() {
        let cases = [
            ("", 0),
            ("hello", 5),
            ("\x1b[1mhello\x1b[0m", 5),
            ("\x1b[32m\x1b[1mhello world\x1b[0m", 11),
            ("中文", 4),
            ("\x1b[34m中文测试\x1b[0m", 8),
            ("Main · CLI Base", 15),
            ("\x1b[1mMain\x1b[0m · \x1b[32mCLI Base\x1b[0m", 15),
            ("Main账号", 8),
        ];

        for (input, want) in cases {
            let got = visible_width(input);
            assert_eq!(got, want, "visible_width({input:?}) = {got}, want {want}");
        }
    }

    #[test]
    fn test_truncate_visible() {
        // 1. Plain ASCII truncation
        let s1 = "Hello World";
        let got1 = truncate_visible(s1, 8);
        assert!(
            visible_width(&got1) <= 8,
            "truncate_visible({s1:?}, 8) exceeded width 8: {got1:?} (w={})",
            visible_width(&got1)
        );
        assert!(
            got1.ends_with('…'),
            "expected '…' suffix on truncated string, got {got1:?}"
        );

        // 2. CJK truncation
        let s2 = "谷歌主账号开发环境";
        let got2 = truncate_visible(s2, 10);
        let w2 = visible_width(&got2);
        assert!(
            w2 <= 10,
            "truncate_visible({s2:?}, 10) exceeded width 10: {got2:?} (w={w2})"
        );
        assert!(got2.ends_with('…'), "expected '…' suffix on {got2:?}");

        // 3. Colored string truncation
        let s3 = "\x1b[32mSuperLongColoredAccountName\x1b[0m";
        let got3 = truncate_visible(s3, 12);
        let w3 = visible_width(&got3);
        assert!(
            w3 <= 12,
            "truncate_visible colored string exceeded width 12: {got3:?} (w={w3})"
        );
        assert!(
            got3.ends_with(crate::cli::CLR_RESET),
            "expected truncated colored string to end with ANSI reset, got {got3:?}"
        );
        assert!(
            got3.contains('…'),
            "expected '…' in truncated colored string, got {got3:?}"
        );

        // 4. String within budget should not be truncated
        let s4 = "Short";
        let got4 = truncate_visible(s4, 10);
        assert_eq!(got4, s4);

        // 5. Zero or negative budget
        assert_eq!(truncate_visible("test", 0), "");
    }

    #[test]
    fn test_strip_ansi() {
        let colored = "\x1b[1m\x1b[32mHello\x1b[0m \x1b[33mWorld\x1b[0m";
        let plain = strip_ansi(colored);
        assert_eq!(plain, "Hello World");
    }
}
