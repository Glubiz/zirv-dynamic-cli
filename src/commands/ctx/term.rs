use super::CtxResult;

pub const STDIN_FD: i32 = 0;

#[cfg(unix)]
#[derive(Debug)]
pub struct RawGuard {
    fd: i32,
    saved: libc::termios,
    active: bool,
}

#[cfg(unix)]
impl RawGuard {
    pub fn enter(fd: i32) -> CtxResult<Self> {
        // SAFETY: `saved` is only read after a successful tcgetattr.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err("tcgetattr failed: stdin is not a terminal".into());
        }
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err("tcsetattr failed: could not enter raw mode".into());
        }
        Ok(Self {
            fd,
            saved,
            active: true,
        })
    }

    /// Idempotent. `panic = "abort"` means Drop is not guaranteed, so callers
    /// invoke this explicitly in every arm that leaves the pump loop.
    pub fn restore(&mut self) -> CtxResult<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) } != 0 {
            return Err("tcsetattr failed: could not restore the terminal".into());
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
pub fn window_size(fd: i32) -> CtxResult<(u16, u16)> {
    // SAFETY: `ws` is only read after a successful ioctl.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) } != 0 {
        return Err("TIOCGWINSZ failed: not a terminal".into());
    }
    Ok((ws.ws_col, ws.ws_row))
}

/// Never constructed: `enter` always fails off unix. It exists so `wrap`
/// compiles and degrades there instead of being cfg'd out entirely.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct RawGuard;

#[cfg(not(unix))]
impl RawGuard {
    pub fn enter(_fd: i32) -> CtxResult<Self> {
        Err("raw terminal mode is only implemented on unix".into())
    }

    pub fn restore(&mut self) -> CtxResult<()> {
        Ok(())
    }
}

#[cfg(not(unix))]
pub fn window_size(_fd: i32) -> CtxResult<(u16, u16)> {
    Err("window size probing is only implemented on unix".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entering_raw_mode_on_a_non_terminal_is_an_error() {
        // CI has no controlling terminal, so this is the path that must be safe.
        let err = RawGuard::enter(-1).expect_err("fd -1 is not a terminal");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn window_size_on_a_non_terminal_is_an_error() {
        assert!(window_size(-1).is_err());
    }

    // portable-pty 0.9's `SlavePty` trait exposes no `as_raw_fd` (only
    // `MasterPty` does), so these two use `/dev/tty` as the terminal fd
    // source instead, per the brief's documented fallback. CI has no
    // controlling terminal, so they skip rather than fail there; the two
    // non-terminal tests above are the CI-visible coverage.
    #[cfg(unix)]
    fn open_controlling_tty() -> i32 {
        unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR) }
    }

    #[cfg(unix)]
    #[test]
    fn restore_is_idempotent() {
        let fd = open_controlling_tty();
        if fd < 0 {
            eprintln!("skipping: no controlling terminal available");
            return;
        }

        let mut guard = RawGuard::enter(fd).expect("raw mode on a tty");
        guard.restore().expect("restore");
        guard.restore().expect("a second restore is a no-op");
        unsafe { libc::close(fd) };
    }

    #[cfg(unix)]
    #[test]
    fn window_size_reads_the_pty_dimensions() {
        let fd = open_controlling_tty();
        if fd < 0 {
            eprintln!("skipping: no controlling terminal available");
            return;
        }
        let (cols, rows) = window_size(fd).expect("size");
        assert!(cols > 0 && rows > 0, "got ({cols}, {rows})");
        unsafe { libc::close(fd) };
    }
}
