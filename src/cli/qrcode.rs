use std::io::Write;

/// Renders an ASCII/Unicode QR code directly to the writer if it is an interactive TTY
/// (or forced via AGY_POOL_QR=1) and wide enough.
pub fn render_terminal_qr_code(w: &mut dyn Write, raw_url: &str, is_tty: bool, term_width: usize) {
    if raw_url.is_empty() {
        return;
    }

    if !is_tty && std::env::var("AGY_POOL_QR").as_deref() != Ok("1") {
        return;
    }

    let code = match qrcode::QrCode::with_error_correction_level(raw_url.as_bytes(), qrcode::EcLevel::L) {
        Ok(c) => c,
        Err(_) => return,
    };

    let qr_str = code.render::<qrcode::render::unicode::Dense1x2>().build();
    let lines: Vec<&str> = qr_str.trim_end_matches('\n').split('\n').collect();
    if lines.is_empty() {
        return;
    }

    let qr_width = lines[0].chars().count();
    if term_width > 0 && term_width < qr_width {
        return;
    }

    let _ = writeln!(
        w,
        "\n[QR Code] Scan with your mobile device to complete Google verification:"
    );
    let _ = writeln!(w, "{qr_str}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_terminal_qr_code_skips_on_empty() {
        let mut buf = Vec::new();
        render_terminal_qr_code(&mut buf, "", false, 100);
        assert!(
            buf.is_empty(),
            "expected empty output for empty URL, got: {:?}",
            String::from_utf8_lossy(&buf)
        );
    }

    #[test]
    fn test_render_terminal_qr_code_forced_via_env() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: test holds TEST_LOCK while modifying environment variable.
        unsafe { std::env::set_var("AGY_POOL_QR", "1") };
        let mut buf = Vec::new();
        render_terminal_qr_code(&mut buf, "https://accounts.google.com/test", false, 100);
        // SAFETY: test holds TEST_LOCK while modifying environment variable.
        unsafe { std::env::remove_var("AGY_POOL_QR") };

        let out = String::from_utf8_lossy(&buf);
        assert!(
            out.contains("[QR Code]"),
            "expected QR Code header in output, got: {out:?}"
        );
        assert!(
            out.contains('█') || out.contains('▀') || out.contains('▄'),
            "expected QR block characters in output, got: {out:?}"
        );
    }

    #[test]
    fn test_render_terminal_qr_code_skips_on_non_tty_by_default() {
        let _lock = crate::config::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: test holds TEST_LOCK while modifying environment variable.
        unsafe { std::env::remove_var("AGY_POOL_QR") };
        let mut buf = Vec::new();
        render_terminal_qr_code(&mut buf, "https://accounts.google.com/test", false, 100);
        assert!(
            buf.is_empty(),
            "expected no output on non-TTY buffer by default, got: {:?}",
            String::from_utf8_lossy(&buf)
        );
    }
}
