/// Returns true if the file descriptor refers to a terminal.
pub fn is_terminal(fd: i32) -> bool {
    // SAFETY: isatty is an FFI call that only checks the validity and tty status of fd.
    unsafe { libc::isatty(fd) == 1 }
}

/// Returns the terminal size (cols, rows) if available.
pub fn get_size(fd: i32) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe {
        // SAFETY: zeroed struct for ioctl output.
        std::mem::zeroed()
    };
    // SAFETY: TIOCGWINSZ queries the terminal size into a valid winsize pointer.
    let ret = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some((ws.ws_col, ws.ws_row))
    } else {
        None
    }
}

/// RAII guard that puts a terminal into raw mode and restores it on Drop.
pub struct RawModeGuard {
    fd: i32,
    orig_termios: libc::termios,
}

impl RawModeGuard {
    pub fn enable(fd: i32) -> Result<Self, std::io::Error> {
        let mut orig: libc::termios = unsafe {
            // SAFETY: zeroed struct for tcgetattr output.
            std::mem::zeroed()
        };
        // SAFETY: tcgetattr gets current terminal attributes for fd.
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut raw = orig;
        // SAFETY: cfmakeraw modifies the termios struct flags in-place.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: tcsetattr sets raw terminal attributes on fd.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(RawModeGuard { fd, orig_termios: orig })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: restores original terminal attributes on fd.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig_termios);
        }
    }
}
