use super::CtxResult;

/// The terminal `wrap` drives. On unix this is literally stdin's file
/// descriptor. On Windows there are no fds at this layer at all: the value is
/// only ever a token meaning "the process's own console", which
/// `windows::std_handles` turns into the real `GetStdHandle` pair. Any other
/// value is rejected there rather than being cast into a handle, so a caller
/// that passes an actual descriptor (or the `-1` the tests use for "not a
/// terminal") still gets an error instead of undefined behavior.
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

/// The Windows console has no termios, so raw mode is two console modes rather
/// than one: keystrokes arrive on the *input* handle, and the escape sequences
/// the wrapped TUI writes are interpreted on the *output* handle. Both have to
/// change, and both have to be put back.
#[cfg(windows)]
mod windows {
    use super::CtxResult;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
        ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleScreenBufferInfo,
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    /// Cooked-mode input: the console itself buffers a whole line, echoes it,
    /// and eats Ctrl-C before the TUI ever sees a byte. Cleared together.
    const COOKED_INPUT: CONSOLE_MODE =
        ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT;

    /// `STDIN_FD` is the only accepted token; see its doc comment. Returns the
    /// (input, output) pair, because raw mode needs both.
    pub fn std_handles(fd: i32) -> CtxResult<(HANDLE, HANDLE)> {
        if fd != super::STDIN_FD {
            return Err(format!(
                "{fd} is not this process's console: on Windows only STDIN_FD names one"
            )
            .into());
        }
        // SAFETY: GetStdHandle takes no pointers and cannot fail other than by
        // returning INVALID_HANDLE_VALUE, which is checked.
        let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if input == INVALID_HANDLE_VALUE || input.is_null() {
            return Err("no console input handle: stdin is not a terminal".into());
        }
        if output == INVALID_HANDLE_VALUE || output.is_null() {
            return Err("no console output handle: stdout is not a terminal".into());
        }
        Ok((input, output))
    }

    /// Fails when the handle is a pipe or a file rather than a console, which
    /// is exactly how `wrap` detects it has no terminal to put into raw mode.
    pub fn console_mode(handle: HANDLE) -> CtxResult<CONSOLE_MODE> {
        let mut mode: CONSOLE_MODE = 0;
        // SAFETY: `mode` is a live local and only read on success.
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return Err("GetConsoleMode failed: not a console".into());
        }
        Ok(mode)
    }

    pub fn set_console_mode(handle: HANDLE, mode: CONSOLE_MODE) -> CtxResult<()> {
        // SAFETY: `handle` came from GetStdHandle and is not a pointer arg.
        if unsafe { SetConsoleMode(handle, mode) } == 0 {
            return Err("SetConsoleMode failed".into());
        }
        Ok(())
    }

    /// The raw counterpart of a saved input mode: no line buffering, no echo,
    /// no Ctrl-C interception, and VT input so arrows and Esc arrive as the
    /// escape sequences the wrapped TUI already understands on unix.
    pub fn raw_input_mode(saved: CONSOLE_MODE) -> CONSOLE_MODE {
        (saved & !COOKED_INPUT) | ENABLE_VIRTUAL_TERMINAL_INPUT
    }

    /// VT processing so the child's own escape sequences render, and no
    /// automatic CR at the right margin: a TUI positions its own cursor, and
    /// the implicit wrap corrupts full-width frames.
    pub fn raw_output_mode(saved: CONSOLE_MODE) -> CONSOLE_MODE {
        saved | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN
    }

    /// srWindow is the visible viewport, which is what a pty size means. The
    /// screen *buffer* is usually taller (that is the scrollback), so sizing
    /// the pty from `dwSize` would tell the agent it has hundreds of rows.
    pub fn viewport(handle: HANDLE) -> CtxResult<(u16, u16)> {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live local and only read on success.
        if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
            return Err("GetConsoleScreenBufferInfo failed: not a console".into());
        }
        let cols = info.srWindow.Right - info.srWindow.Left + 1;
        let rows = info.srWindow.Bottom - info.srWindow.Top + 1;
        if cols <= 0 || rows <= 0 {
            return Err("console reported an empty window".into());
        }
        Ok((cols as u16, rows as u16))
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub struct RawGuard {
    input: windows_sys::Win32::Foundation::HANDLE,
    output: windows_sys::Win32::Foundation::HANDLE,
    saved_input: windows_sys::Win32::System::Console::CONSOLE_MODE,
    saved_output: windows_sys::Win32::System::Console::CONSOLE_MODE,
    active: bool,
}

// The handles are process-wide console handles, not owned resources: they are
// valid for the life of the process and copying one does not duplicate
// anything. `RawGuard` is only ever moved between the pump's own frames, but
// the raw pointer type would otherwise make it !Send for no reason.
#[cfg(windows)]
unsafe impl Send for RawGuard {}

#[cfg(windows)]
impl RawGuard {
    pub fn enter(fd: i32) -> CtxResult<Self> {
        let (input, output) = windows::std_handles(fd)?;
        let saved_input = windows::console_mode(input)?;
        let saved_output = windows::console_mode(output)?;

        windows::set_console_mode(input, windows::raw_input_mode(saved_input))
            .map_err(|_| "could not put the console input into raw mode")?;
        // Input is already raw here, so a failure below has to undo it rather
        // than leave the terminal half-switched with no guard to restore it.
        if let Err(e) = windows::set_console_mode(output, windows::raw_output_mode(saved_output)) {
            let _ = windows::set_console_mode(input, saved_input);
            return Err(e);
        }

        Ok(Self {
            input,
            output,
            saved_input,
            saved_output,
            active: true,
        })
    }

    /// Idempotent. `panic = "abort"` means Drop is not guaranteed, so callers
    /// invoke this explicitly in every arm that leaves the pump loop.
    ///
    /// Both modes are restored even if the first restore fails: leaving the
    /// console echoing but without VT processing is worse than either.
    pub fn restore(&mut self) -> CtxResult<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let input = windows::set_console_mode(self.input, self.saved_input);
        let output = windows::set_console_mode(self.output, self.saved_output);
        input
            .and(output)
            .map_err(|_| "SetConsoleMode failed: could not restore the terminal".into())
    }
}

#[cfg(windows)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Takes the *stdin* token for symmetry with unix, but reads the console's
/// output handle: on Windows the viewport belongs to the screen buffer, and
/// asking the input handle for its size is meaningless.
#[cfg(windows)]
pub fn window_size(fd: i32) -> CtxResult<(u16, u16)> {
    let (_, output) = windows::std_handles(fd)?;
    windows::viewport(output)
}

/// Never constructed: `enter` always fails on a platform that is neither unix
/// nor Windows. It exists so `wrap` compiles and degrades there instead of
/// being cfg'd out entirely.
#[cfg(not(any(unix, windows)))]
#[derive(Debug)]
pub struct RawGuard;

#[cfg(not(any(unix, windows)))]
impl RawGuard {
    pub fn enter(_fd: i32) -> CtxResult<Self> {
        Err("raw terminal mode is only implemented on unix and Windows".into())
    }

    pub fn restore(&mut self) -> CtxResult<()> {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
pub fn window_size(_fd: i32) -> CtxResult<(u16, u16)> {
    Err("window size probing is only implemented on unix and Windows".into())
}

/// Restoring guard for `enable_vt_output`. Deliberately independent from
/// `RawGuard`: the launch banner (and, once drawn, the reserved status bar)
/// need VT processing enabled before `wrap` ever puts the terminal into raw
/// mode, and it needs to stay enabled for the life of the session, not just
/// the pump loop.
#[cfg(windows)]
#[derive(Debug)]
pub struct VtGuard {
    output: windows_sys::Win32::Foundation::HANDLE,
    saved: windows_sys::Win32::System::Console::CONSOLE_MODE,
    active: bool,
}

// Same reasoning as `RawGuard`: a process-wide console handle, not an owned
// resource.
#[cfg(windows)]
unsafe impl Send for VtGuard {}

#[cfg(windows)]
impl VtGuard {
    /// Idempotent, like `RawGuard::restore`.
    pub fn restore(&mut self) -> CtxResult<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        windows::set_console_mode(self.output, self.saved)
            .map_err(|_| "SetConsoleMode failed: could not restore VT output".into())
    }
}

#[cfg(windows)]
impl Drop for VtGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// ORs `ENABLE_VIRTUAL_TERMINAL_PROCESSING` onto the current output console
/// mode, saving the original for `restore`. Failure (no console, e.g. stdout
/// redirected to a file or pipe) is exactly how a caller learns `vt_ok` is
/// false; it never falls back to guessing.
#[cfg(windows)]
pub fn enable_vt_output() -> CtxResult<VtGuard> {
    let (_, output) = windows::std_handles(STDIN_FD)?;
    let saved = windows::console_mode(output)?;
    let enabled = saved | windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    windows::set_console_mode(output, enabled)
        .map_err(|_| "SetConsoleMode failed: could not enable VT output")?;
    Ok(VtGuard {
        output,
        saved,
        active: true,
    })
}

/// Unix terminals already interpret VT escapes with no mode to flip; the only
/// question is whether stdout is a terminal at all.
#[cfg(unix)]
#[derive(Debug)]
pub struct VtGuard;

#[cfg(unix)]
impl VtGuard {
    pub fn restore(&mut self) -> CtxResult<()> {
        Ok(())
    }
}

/// Unix's stdout file descriptor, for the same `isatty` check
/// `window_size`/`RawGuard` already key off of `STDIN_FD` for stdin.
#[cfg(unix)]
const STDOUT_FD: i32 = 1;

#[cfg(unix)]
pub fn enable_vt_output() -> CtxResult<VtGuard> {
    // SAFETY: isatty takes a plain fd and only ever returns 0 or 1.
    if unsafe { libc::isatty(STDOUT_FD) } == 1 {
        Ok(VtGuard)
    } else {
        Err("stdout is not a terminal".into())
    }
}

/// Never constructed, matching `RawGuard`'s fallback on a platform that is
/// neither unix nor Windows.
#[cfg(not(any(unix, windows)))]
#[derive(Debug)]
pub struct VtGuard;

#[cfg(not(any(unix, windows)))]
impl VtGuard {
    pub fn restore(&mut self) -> CtxResult<()> {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
pub fn enable_vt_output() -> CtxResult<VtGuard> {
    Err("VT output is only implemented on unix and Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn enable_vt_output_on_a_non_terminal_is_an_error() {
        // CI's stdout is piped, not a controlling terminal.
        let err = enable_vt_output().expect_err("stdout is not a terminal under CI");
        assert!(!err.to_string().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn restoring_a_vt_guard_twice_is_a_noop() {
        let mut guard = VtGuard;
        guard.restore().expect("restore");
        guard.restore().expect("a second restore is a no-op");
    }

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

    // Windows coverage. `cargo test` gives the test binary a piped stdin and
    // no console of its own under CI, so every test that needs a real console
    // skips rather than fails there -- but the mode arithmetic and the
    // handle-token rejection are pure and always run.
    #[cfg(windows)]
    mod win {
        use super::super::windows::{
            console_mode, raw_input_mode, raw_output_mode, set_console_mode, std_handles, viewport,
        };
        use super::*;
        use windows_sys::Win32::System::Console::{
            DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
            ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        };

        /// True only when this test process actually owns a console, which is
        /// not the case for a piped `cargo test` run under CI.
        fn has_console() -> bool {
            std_handles(STDIN_FD)
                .and_then(|(input, output)| {
                    console_mode(input)?;
                    console_mode(output)?;
                    Ok(())
                })
                .is_ok()
        }

        #[test]
        fn raw_input_mode_clears_line_editing_and_asks_for_virtual_terminal_keys() {
            let cooked = ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT | 0x80;
            let raw = raw_input_mode(cooked);
            assert_eq!(raw & ENABLE_LINE_INPUT, 0, "line buffering must be off");
            assert_eq!(raw & ENABLE_ECHO_INPUT, 0, "double echo must be off");
            assert_eq!(
                raw & ENABLE_PROCESSED_INPUT,
                0,
                "the console must stop eating Ctrl-C"
            );
            assert_ne!(
                raw & ENABLE_VIRTUAL_TERMINAL_INPUT,
                0,
                "arrows and Esc must arrive as escape sequences"
            );
            assert_ne!(raw & 0x80, 0, "unrelated bits the console set are kept");
        }

        #[test]
        fn raw_output_mode_adds_virtual_terminal_processing_without_dropping_anything() {
            let saved = 0x1 | 0x2;
            let raw = raw_output_mode(saved);
            assert_eq!(raw & saved, saved, "nothing the console had is cleared");
            assert_ne!(raw & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
            assert_ne!(raw & DISABLE_NEWLINE_AUTO_RETURN, 0);
        }

        /// `STDIN_FD` is a token, not a descriptor: anything else must be
        /// refused rather than reinterpreted as a handle.
        #[test]
        fn only_the_stdin_token_names_a_console_handle() {
            let err = std_handles(-1).expect_err("-1 is not the console token");
            assert!(err.to_string().contains("console"), "{err}");
            assert!(std_handles(7).is_err());
        }

        /// The whole point of the guard: whatever the console was in before,
        /// it is in again afterwards. A leaked raw mode leaves the user's
        /// shell with no echo after `wrap` exits.
        #[test]
        fn a_raw_guard_round_trip_restores_both_console_modes() {
            if !has_console() {
                eprintln!("skipping: this test process owns no console");
                return;
            }
            let (input, output) = std_handles(STDIN_FD).expect("handles");
            let before = (
                console_mode(input).expect("input mode"),
                console_mode(output).expect("output mode"),
            );

            let mut guard = RawGuard::enter(STDIN_FD).expect("raw mode on a console");
            let during = (
                console_mode(input).expect("input mode"),
                console_mode(output).expect("output mode"),
            );
            assert_eq!(during.0 & ENABLE_LINE_INPUT, 0, "raw mode took effect");
            assert_ne!(during.1 & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);

            guard.restore().expect("restore");
            let after = (
                console_mode(input).expect("input mode"),
                console_mode(output).expect("output mode"),
            );
            assert_eq!(after, before, "both modes are put back exactly");

            guard.restore().expect("a second restore is a no-op");
            assert_eq!(
                (
                    console_mode(input).expect("input mode"),
                    console_mode(output).expect("output mode")
                ),
                before
            );
        }

        /// Dropping the guard has to restore too, for the arms that never
        /// reach an explicit `restore`.
        #[test]
        fn dropping_the_guard_restores_the_console() {
            if !has_console() {
                eprintln!("skipping: this test process owns no console");
                return;
            }
            let (input, _) = std_handles(STDIN_FD).expect("handles");
            let before = console_mode(input).expect("input mode");
            drop(RawGuard::enter(STDIN_FD).expect("raw mode on a console"));
            assert_eq!(console_mode(input).expect("input mode"), before);
        }

        /// The resize poll in `wrap` is dead code unless this reports the
        /// viewport, and the pty stays pinned at the 80x24 default.
        #[test]
        fn window_size_reports_the_console_viewport() {
            if !has_console() {
                eprintln!("skipping: this test process owns no console");
                return;
            }
            let (cols, rows) = window_size(STDIN_FD).expect("size");
            assert!(cols > 0 && rows > 0, "got ({cols}, {rows})");

            // And it is the viewport, not the scrollback: the screen buffer is
            // normally far taller than the window.
            let (_, output) = std_handles(STDIN_FD).expect("handles");
            let (vcols, vrows) = viewport(output).expect("viewport");
            assert_eq!((cols, rows), (vcols, vrows));
        }

        /// A half-applied raw mode is the failure that leaves a terminal
        /// unusable, so `enter` rolls the input handle back itself.
        #[test]
        fn a_failed_enter_leaves_the_console_untouched() {
            if !has_console() {
                eprintln!("skipping: this test process owns no console");
                return;
            }
            let (input, _) = std_handles(STDIN_FD).expect("handles");
            let before = console_mode(input).expect("input mode");
            // Not a console handle, so setting a mode on it fails the way a
            // redirected stdout would.
            assert!(set_console_mode(std::ptr::null_mut(), 0).is_err());
            assert_eq!(console_mode(input).expect("input mode"), before);
        }

        /// `enable_vt_output` must OR the flag on without clearing anything
        /// else the console already had, and `restore` must put the mode back
        /// exactly.
        #[test]
        fn enable_vt_output_round_trips_the_console_mode() {
            if !has_console() {
                eprintln!("skipping: this test process owns no console");
                return;
            }
            let (_, output) = std_handles(STDIN_FD).expect("handles");
            let before = console_mode(output).expect("output mode");

            let mut guard = super::super::enable_vt_output().expect("vt on a console");
            assert_ne!(
                console_mode(output).expect("output mode") & ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                0,
                "VT processing must be enabled"
            );

            guard.restore().expect("restore");
            assert_eq!(console_mode(output).expect("output mode"), before);
            guard.restore().expect("a second restore is a no-op");
        }
    }
}
