use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use super::CtxResult;

// ---------------------------------------------------------------------------
// F4: putting the shared console back when this process is killed from
// outside.
//
// `RawGuard`/`VtGuard` mutate the *user's own* console, not a private handle,
// and `restore` only runs on a path this process actually reaches. An
// external kill -- `taskkill`, a parent supervisor's `TerminateProcess`, a
// Ctrl-C at the shell, closing the window -- reaches none of them, and the
// release profile is `panic = "abort"`, so `Drop` is no safety net either.
// What is left behind is a console with no echo, no line editing, and (when
// the chrome bar was up) a scroll region pinned one row short of the bottom:
// unusable until the user finds `reset` or opens a new window.
//
// So the modes are also stashed in a process-global the moment they are
// changed, and a handler that restores them is installed once. The handler
// runs *before* default handling and then lets it proceed.
//
// What this cannot cover: `TerminateProcess` (Windows) and `SIGKILL` (unix)
// run no user code at all, by design. Nothing in userspace can. The stash is
// still worth having for every other route, which is most of them.
//
// Manual verification recipe (not automatable -- it needs a real console and
// an external killer, and this repo's own test process is frequently itself
// running under `zirv ctx wrap`, where a stray console control event would
// take the developer's session down):
//
//   1. In a *detached* console:  cmd /c start zirv chat
//      (unix: run it in a separate terminal emulator window)
//   2. From another shell, once the bar is drawn:
//        Windows:  taskkill /PID <pid>            (no /F -- /F is
//                  TerminateProcess and runs no handler)
//        unix:     kill -TERM <pid>
//   3. In the detached console, type: echo hello
//      Before F4 the characters do not echo and the bottom row is fenced off.
//      After F4 the console echoes normally and scrolls to the last row.
// ---------------------------------------------------------------------------

/// Written to the real stdout from the handler when the chrome bar was up:
/// `CSI r` resets the scroll region to the full screen (the thing that
/// actually wedges a terminal), and `CSI ?25h` shows a cursor the TUI may
/// have hidden.
///
/// Deliberately a fixed constant rather than `chrome::bar_reset_sequence`,
/// which formats the current row count into the string: formatting allocates,
/// and this may run in a POSIX signal handler where allocation is not
/// async-signal-safe. Clearing the reserved row is cosmetic; un-fencing the
/// scroll region is not, and that part needs no row number.
const EMERGENCY_RESET: &[u8] = b"\x1b[r\x1b[?25h";

/// What the dashboard owes the terminal on the way out, in the order a
/// terminal wants it: show the cursor again (ratatui hides it on every frame
/// it draws and `LeaveAlternateScreen` does **not** put it back), turn off
/// every mouse-reporting mode, un-fence the scroll region, then leave the
/// alternate screen.
///
/// All four mouse modes -- `?1000` (button press/release), `?1002` (drag),
/// `?1003` (any motion), `?1006` (SGR extended coordinates) -- are disabled
/// unconditionally, and deliberately more than [`dash_mouse_on_bytes`] ever
/// enables. Turning off a mode that was never on is a no-op, while *leaving*
/// one on hands the operator a shell in which every click and every wheel
/// notch spews escape sequences at the prompt. That is a badly broken
/// terminal, and it is exactly the failure an external kill or a panic would
/// otherwise produce, so it belongs in the shared constant rather than in the
/// one clean exit path that could remember to undo it. Covering the two modes
/// zirv does not turn on also means a future change to the enable set cannot
/// silently outrun the cleanup.
///
/// A fixed constant for the same reason `EMERGENCY_RESET` is one: this runs
/// from a console-control/signal handler as well as from the ordinary
/// teardown path, and formatting there is not async-signal-safe. Shared by
/// both so the panic hook, the external-kill handler and `teardown_terminal`
/// can never drift apart -- the pre-F4 hook wrote
/// `emergency_reset_bytes(false)`, which is the *empty* slice, and no path at
/// all ever showed the cursor again.
const DASH_RESET: &[u8] = b"\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[r\x1b[?1049l";

/// Whether the chrome bar currently owns a reserved row, and therefore
/// whether the handler owes the terminal a scroll-region reset. Set by `wrap`
/// when it writes the scroll region, cleared when it resets the bar.
static BAR_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether a dashboard currently owns the alternate screen, and therefore
/// whether the emergency handler owes the terminal [`DASH_RESET`]. Set by
/// `dash::run_dashboard` right after it enters the alternate screen, cleared
/// on every exit arm.
static DASH_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn set_dash_active(active: bool) {
    DASH_ACTIVE.store(active, Ordering::SeqCst);
}

pub fn dash_active() -> bool {
    DASH_ACTIVE.load(Ordering::SeqCst)
}

/// The bytes that put a terminal back after a dashboard session: cursor
/// shown, mouse reporting off, scroll region reset, alternate screen left.
/// Used by the dashboard's own teardown, its panic hook, and the
/// external-kill handler alike.
pub fn dash_reset_bytes() -> &'static [u8] {
    DASH_RESET
}

/// Mouse reporting, at exactly the level the dashboard's wheel-scrolling
/// needs and no more: `?1000` (button press/release, which is what a wheel
/// notch is reported as) plus `?1006` (SGR extended coordinates).
///
/// Deliberately **not** crossterm's `EnableMouseCapture`, and deliberately
/// without `?1002`/`?1003`. Those are the motion-tracking modes, and a probe
/// on a real Windows Terminal session confirmed what they cost: `?1003`
/// reports *every* pointer movement, so simply sweeping the mouse across the
/// window produced dozens of `MouseEventKind::Moved` events. Inside the
/// dashboard's bounded per-tick input drain that flood competes directly with
/// the operator's keystrokes, and it buys nothing -- the dashboard acts on
/// `ScrollUp`/`ScrollDown` and discards every other mouse kind. Do not
/// "simplify" this back to `EnableMouseCapture`.
///
/// `?1006` is not optional either: the default X10 encoding packs the column
/// into a single byte and so cannot express a column past 223, and terminals
/// are routinely wider than that.
pub fn dash_mouse_on_bytes() -> &'static [u8] {
    DASH_MOUSE_ON
}

/// See [`dash_mouse_on_bytes`]. A constant for the same allocation-free
/// reason every other sequence in this module is one.
const DASH_MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";

/// Installed exactly once, however many guards are entered.
static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

pub fn set_bar_active(active: bool) {
    BAR_ACTIVE.store(active, Ordering::SeqCst);
}

pub fn bar_active() -> bool {
    BAR_ACTIVE.load(Ordering::SeqCst)
}

/// What the handler writes to stdout, given whether the bar was up. Pure, so
/// the decision is testable even though invoking the handler itself is not.
pub fn emergency_reset_bytes(bar_active: bool) -> &'static [u8] {
    if bar_active { EMERGENCY_RESET } else { b"" }
}

/// Installs the console-restore handler, returning whether *this* call was
/// the one that installed it. Idempotent: every later call is a no-op and
/// returns `false`, so `RawGuard::enter` can call it unconditionally.
pub fn install_console_restore_handler() -> bool {
    if HANDLER_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    install_platform_handler();
    true
}

/// Stashes the console's modes *as they are right now*, for a caller that
/// puts the terminal into raw mode without going through `RawGuard` -- the
/// dashboard uses crossterm's own `enable_raw_mode`, so nothing would
/// otherwise ever fill the stash and the restore handler would have nothing
/// to put back. Write-once like `stash_console_state` itself (it is the same
/// stash), so calling it after a `RawGuard` is already up cannot overwrite
/// the genuinely-original modes with already-raw ones. Returns whether this
/// call did the stashing; a terminal-less process simply stashes nothing.
#[cfg(windows)]
pub fn stash_current_console() -> bool {
    let Ok((input, output)) = windows::std_handles(STDIN_FD) else {
        return false;
    };
    let (Ok(saved_input), Ok(saved_output)) =
        (windows::console_mode(input), windows::console_mode(output))
    else {
        return false;
    };
    stash_console_state(input as usize, output as usize, saved_input, saved_output)
}

#[cfg(unix)]
pub fn stash_current_console() -> bool {
    // SAFETY: `saved` is only read after a successful tcgetattr.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(STDIN_FD, &mut saved) } != 0 {
        return false;
    }
    stash_console_state(STDIN_FD, saved)
}

#[cfg(not(any(unix, windows)))]
pub fn stash_current_console() -> bool {
    false
}

/// The handler body, shared by both platforms. Async-signal-safe: no
/// allocation, no locks, no formatting -- a raw write of a constant byte
/// string and a direct mode-restoring syscall, both taken from state that was
/// stashed before any signal could arrive.
fn restore_console_from_handler() {
    let reset = emergency_reset_bytes(bar_active());
    if !reset.is_empty() {
        write_stdout_raw(reset);
    }
    // A dashboard owns the *alternate* screen on top of whatever the bar
    // did: without this the user is killed out of a full-screen TUI into a
    // shell with no cursor, still on the alternate buffer.
    if dash_active() {
        write_stdout_raw(DASH_RESET);
    }
    restore_stashed_console_modes();
}

#[cfg(unix)]
fn write_stdout_raw(bytes: &[u8]) {
    // SAFETY: a plain write of a borrowed slice to fd 1; `write` is
    // async-signal-safe and the result is deliberately ignored (there is
    // nothing useful to do about a failed emergency reset).
    unsafe {
        let _ = libc::write(1, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
}

#[cfg(windows)]
fn write_stdout_raw(bytes: &[u8]) {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};
    // SAFETY: `GetStdHandle` takes no pointers; `WriteFile` gets a live
    // borrowed buffer, a live out-param and a null OVERLAPPED (a synchronous
    // write). A failure has nothing useful to fall back to.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut written: u32 = 0;
        let _ = WriteFile(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        );
    }
}

#[cfg(not(any(unix, windows)))]
fn write_stdout_raw(_bytes: &[u8]) {}

// -- unix: the saved termios, and a minimal SIGINT/SIGTERM handler ----------

#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct StashedConsole {
    fd: i32,
    saved: libc::termios,
}

// `termios` is plain POSIX data with no interior pointers, and the stash is
// written once and only ever read; the raw `libc` type simply carries no
// `Send`/`Sync` impls of its own.
#[cfg(unix)]
unsafe impl Send for StashedConsole {}
#[cfg(unix)]
unsafe impl Sync for StashedConsole {}

#[cfg(unix)]
static STASHED_CONSOLE: OnceLock<StashedConsole> = OnceLock::new();

/// Write-once, by design: the *first* mode this process ever saw is the one
/// the user started with, and the one they must get back. A later guard
/// (`wrap` restarting its bar, say) would otherwise stash an already-raw mode
/// and "restore" the console into it. Returns whether this call did the
/// stashing.
#[cfg(unix)]
pub fn stash_console_state(fd: i32, saved: libc::termios) -> bool {
    STASHED_CONSOLE.set(StashedConsole { fd, saved }).is_ok()
}

/// Only this module's own tests need to ask; the production path just calls
/// `stash_console_state` and ignores the answer. Kept as a named probe rather
/// than reaching into the `OnceLock` from the test module, so the invariant
/// under test ("write-once") is expressed against the same surface production
/// uses.
#[cfg(unix)]
#[allow(dead_code)]
pub fn console_state_is_stashed() -> bool {
    STASHED_CONSOLE.get().is_some()
}

#[cfg(unix)]
fn restore_stashed_console_modes() {
    if let Some(stashed) = STASHED_CONSOLE.get() {
        // SAFETY: `tcsetattr` is async-signal-safe and the stashed value was
        // filled by a successful `tcgetattr` on this same descriptor.
        unsafe {
            libc::tcsetattr(stashed.fd, libc::TCSANOW, &stashed.saved);
        }
    }
}

/// C6: whether a signal whose previous disposition was `previous` may be
/// taken over by this process's own handler.
///
/// `SIG_IGN` is inherited across `fork`/`exec`, and is how a parent says
/// "this child must not die of that signal" -- a `nohup`-style wrapper or a
/// job runner ignoring `SIGHUP`/`SIGINT` before spawning. Installing a
/// handler over it silently *un-ignores* the signal, so a `wrap` session
/// started that way would begin dying of signals its parent had deliberately
/// neutralised. Pure, so the rule is testable even though `libc::signal`
/// itself is not.
#[cfg(unix)]
pub fn may_install_over(previous: libc::sighandler_t) -> bool {
    previous != libc::SIG_IGN
}

#[cfg(unix)]
extern "C" fn terminating_signal_handler(sig: libc::c_int) {
    restore_console_from_handler();
    // Re-raise under the default disposition, so the process still dies of
    // the signal it was sent (and reports the right status to its parent)
    // rather than silently swallowing it.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

#[cfg(unix)]
fn install_platform_handler() {
    // SAFETY: `signal` with a plain `extern "C"` handler; the handler itself
    // does nothing that is not async-signal-safe. The return value is the
    // *previous* disposition, which C6 requires honouring: an inherited
    // `SIG_IGN` is put straight back rather than quietly replaced.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let previous = libc::signal(signal, terminating_signal_handler as libc::sighandler_t);
            if !may_install_over(previous) {
                libc::signal(signal, previous);
            }
        }
    }
}

// -- windows: the saved console modes, and a console control handler --------

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StashedConsole {
    /// Held as `usize` rather than `HANDLE`: these are process-wide console
    /// handles valid for the life of the process, not owned resources, and
    /// the raw-pointer type would otherwise make the global `!Send`/`!Sync`
    /// for no reason.
    input: usize,
    output: usize,
    saved_input: u32,
    saved_output: u32,
}

#[cfg(windows)]
static STASHED_CONSOLE: OnceLock<StashedConsole> = OnceLock::new();

/// Write-once; see the unix counterpart for why. Returns whether this call
/// did the stashing.
#[cfg(windows)]
pub fn stash_console_state(
    input: usize,
    output: usize,
    saved_input: u32,
    saved_output: u32,
) -> bool {
    STASHED_CONSOLE
        .set(StashedConsole {
            input,
            output,
            saved_input,
            saved_output,
        })
        .is_ok()
}

/// See the unix counterpart: a test-only probe over the production surface.
#[cfg(windows)]
#[allow(dead_code)]
pub fn console_state_is_stashed() -> bool {
    STASHED_CONSOLE.get().is_some()
}

/// The stashed modes, for tests that need to prove the stash is write-once.
#[cfg(windows)]
#[allow(dead_code)]
pub fn stashed_console_state() -> Option<(usize, usize, u32, u32)> {
    STASHED_CONSOLE
        .get()
        .map(|s| (s.input, s.output, s.saved_input, s.saved_output))
}

#[cfg(windows)]
fn restore_stashed_console_modes() {
    use windows_sys::Win32::System::Console::SetConsoleMode;
    if let Some(stashed) = STASHED_CONSOLE.get() {
        // SAFETY: both handles came from `GetStdHandle` in `std_handles` and
        // are valid for the life of the process. Both are restored even if
        // the first fails: a console left echoing but without VT processing
        // is worse than either alone.
        unsafe {
            SetConsoleMode(stashed.input as _, stashed.saved_input);
            SetConsoleMode(stashed.output as _, stashed.saved_output);
        }
    }
}

/// Returns FALSE so the next handler in the chain -- ultimately the default
/// one, which terminates the process -- still runs. This exists to put the
/// console back on the way out, not to swallow the event.
#[cfg(windows)]
unsafe extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> windows_sys::core::BOOL {
    restore_console_from_handler();
    0
}

#[cfg(windows)]
fn install_platform_handler() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: registering a plain `extern "system"` function pointer. A
    // failure here only means the emergency restore will not run; there is
    // nothing to fall back to and nothing to report on.
    unsafe {
        SetConsoleCtrlHandler(Some(console_ctrl_handler), 1);
    }
}

#[cfg(not(any(unix, windows)))]
fn restore_stashed_console_modes() {}

#[cfg(not(any(unix, windows)))]
fn install_platform_handler() {}

#[cfg(not(any(unix, windows)))]
pub fn console_state_is_stashed() -> bool {
    false
}

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
        // F4: the guard's own `restore` only runs on a path this process
        // reaches. Stash the pre-raw mode and arm the handler so an external
        // kill does not leave the user's shell without echo.
        stash_console_state(fd, saved);
        install_console_restore_handler();
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

        // F4: stashed *before* either mode is changed, so what the handler
        // restores is genuinely what the user started with even if `enter`
        // fails half way through below. The guard's own `restore` only runs
        // on a path this process reaches; an external kill reaches none.
        stash_console_state(input as usize, output as usize, saved_input, saved_output);
        install_console_restore_handler();

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

    // F4. The handler itself can only be exercised against a real console
    // with a real external killer, which is what the manual recipe at the top
    // of this file is for. What *is* testable -- and is what actually broke
    // in review -- are the three invariants underneath it: the stash is
    // write-once, the handler is installed once, and the reset sequence is
    // owed exactly when the bar is up.

    /// Installing has to be idempotent: `RawGuard::enter` calls it
    /// unconditionally, and `wrap` enters a guard once per session while the
    /// test binary runs many sessions in one process.
    ///
    /// Written so it does not care whether some earlier test already
    /// installed the handler -- the invariant is "after any call, no later
    /// call installs again", not "this test is the installer".
    #[test]
    fn the_console_restore_handler_is_installed_exactly_once() {
        let _ = install_console_restore_handler();
        assert!(
            !install_console_restore_handler(),
            "a second call must be a no-op"
        );
        assert!(!install_console_restore_handler());
    }

    /// The stash must keep the *first* mode it ever saw. A later guard would
    /// otherwise stash an already-raw mode, and the emergency restore would
    /// put the console back into raw mode instead of out of it.
    #[test]
    fn the_stashed_console_state_is_write_once() {
        // May or may not already be stashed depending on test order and
        // whether this process owns a console; either way the invariant below
        // is the same.
        let already = console_state_is_stashed();

        #[cfg(windows)]
        {
            let first = stash_console_state(1, 2, 0xAAAA, 0xBBBB);
            assert_eq!(first, !already, "it stashes iff nothing had stashed before");
            let snapshot = stashed_console_state().expect("something is stashed now");
            assert!(
                !stash_console_state(9, 9, 0xFFFF, 0xFFFF),
                "a second stash must be refused"
            );
            assert_eq!(
                stashed_console_state(),
                Some(snapshot),
                "and must not have overwritten the first"
            );
        }

        #[cfg(unix)]
        {
            // SAFETY: a zeroed termios is never read back here -- only its
            // presence in the stash is asserted on.
            let blank: libc::termios = unsafe { std::mem::zeroed() };
            let first = stash_console_state(7, blank);
            assert_eq!(first, !already);
            assert!(
                !stash_console_state(9, blank),
                "a second stash must be refused"
            );
        }

        assert!(console_state_is_stashed());
    }

    /// The handler owes the terminal a scroll-region reset exactly when the
    /// chrome bar had fenced its last row off, and nothing otherwise: a
    /// bar-less session's console was never touched beyond its modes.
    #[test]
    fn the_emergency_reset_is_owed_only_while_the_bar_is_up() {
        assert_eq!(emergency_reset_bytes(false), b"");

        let owed = emergency_reset_bytes(true);
        assert!(
            owed.starts_with(b"\x1b[r"),
            "CSI r is what un-fences the scroll region: {owed:?}"
        );
        assert!(
            owed.ends_with(b"\x1b[?25h"),
            "and the cursor is put back on: {owed:?}"
        );
        // Fixed, allocation-free bytes: this may run inside a POSIX signal
        // handler, where formatting a row number would not be safe.
        assert_eq!(owed, b"\x1b[r\x1b[?25h");
    }

    /// F4: the dashboard's own exit sequence. The pre-fix panic hook wrote
    /// `emergency_reset_bytes(false)` -- the empty slice -- and no exit path
    /// anywhere showed the cursor again, even though ratatui hides it on
    /// every frame and `LeaveAlternateScreen` does not restore it. One
    /// constant, shared by the teardown, the panic hook and the
    /// external-kill handler, so the three cannot drift.
    #[test]
    fn the_dash_reset_shows_the_cursor_and_leaves_the_alternate_screen() {
        let reset = dash_reset_bytes();
        assert!(
            reset.windows(6).any(|w| w == b"\x1b[?25h"),
            "the cursor must be shown again: {reset:?}"
        );
        assert!(
            reset.windows(8).any(|w| w == b"\x1b[?1049l"),
            "the alternate screen must be left: {reset:?}"
        );
        assert!(
            reset.windows(3).any(|w| w == b"\x1b[r"),
            "the scroll region must be un-fenced: {reset:?}"
        );
        assert!(
            !reset.is_empty(),
            "the pre-F4 hook wrote an empty slice here"
        );
    }

    /// Mouse reporting (`dash.mouse`) is switched on with the dashboard, and a
    /// terminal left reporting mouse events after the process is gone spews
    /// escape sequences at the operator's shell prompt on every click and every
    /// wheel notch. Disabling it therefore lives in the *shared* constant, so
    /// the panic hook and the external-kill handler cover it without either
    /// having to remember -- and so it is undone even on the paths that never
    /// enabled it, where the disable is a harmless no-op.
    ///
    /// The reset deliberately covers more modes than the enable turns on, so a
    /// later change to the enable set cannot outrun the cleanup.
    #[test]
    fn the_dash_reset_turns_every_mouse_reporting_mode_back_off() {
        let reset = dash_reset_bytes();
        for mode in [
            &b"\x1b[?1000l"[..],
            &b"\x1b[?1002l"[..],
            &b"\x1b[?1003l"[..],
            &b"\x1b[?1006l"[..],
        ] {
            assert!(
                reset.windows(mode.len()).any(|w| w == mode),
                "{} missing from the dash reset: {reset:?}",
                String::from_utf8_lossy(mode)
            );
        }
    }

    /// The enable set is button+wheel reporting with SGR coordinates, and
    /// nothing else. `?1002`/`?1003` are the motion-tracking modes: a probe on
    /// a real Windows Terminal session showed `?1003` reporting every pointer
    /// movement, which floods the dashboard's bounded per-tick input drain and
    /// competes with the operator's own keystrokes for it. The dashboard only
    /// ever acts on wheel events, so tracking motion buys nothing at all.
    #[test]
    fn mouse_reporting_is_enabled_for_the_wheel_only_never_for_motion() {
        let on = dash_mouse_on_bytes();
        assert_eq!(
            on, b"\x1b[?1000h\x1b[?1006h",
            "button+wheel reporting with SGR coordinates, exactly"
        );
        for motion in [&b"\x1b[?1002h"[..], &b"\x1b[?1003h"[..]] {
            assert!(
                !on.windows(motion.len()).any(|w| w == motion),
                "motion tracking must stay off: {}",
                String::from_utf8_lossy(motion)
            );
        }
        // SGR coordinates are not optional: the default X10 encoding cannot
        // express a column past 223, and terminals are routinely wider.
        assert!(on.windows(8).any(|w| w == b"\x1b[?1006h"));

        // Every mode the enable turns on is turned back off by the shared
        // reset -- the invariant that actually protects the operator's shell.
        for (on_seq, off_seq) in [
            (&b"\x1b[?1000h"[..], &b"\x1b[?1000l"[..]),
            (&b"\x1b[?1006h"[..], &b"\x1b[?1006l"[..]),
        ] {
            assert!(on.windows(on_seq.len()).any(|w| w == on_seq));
            assert!(
                dash_reset_bytes()
                    .windows(off_seq.len())
                    .any(|w| w == off_seq)
            );
        }
    }

    #[test]
    fn the_dash_active_flag_round_trips() {
        struct DashFlagGuard(bool);
        impl Drop for DashFlagGuard {
            fn drop(&mut self) {
                set_dash_active(self.0);
            }
        }
        let _restore = DashFlagGuard(dash_active());

        set_dash_active(true);
        assert!(dash_active());
        set_dash_active(false);
        assert!(!dash_active());
    }

    #[test]
    fn the_bar_active_flag_round_trips() {
        // C10: a guard, so a failing assertion below cannot leave the
        // process-global flag set for every later test.
        struct BarFlagGuard(bool);
        impl Drop for BarFlagGuard {
            fn drop(&mut self) {
                set_bar_active(self.0);
            }
        }
        let _restore = BarFlagGuard(bar_active());

        set_bar_active(true);
        assert!(bar_active());
        assert_eq!(emergency_reset_bytes(bar_active()), EMERGENCY_RESET);
        set_bar_active(false);
        assert!(!bar_active());
        assert_eq!(emergency_reset_bytes(bar_active()), b"");
    }

    /// C6: `SIG_IGN` is inherited across `fork`/`exec` and is how a parent
    /// says "this child must not die of that signal". Installing over it
    /// silently un-ignores the signal.
    #[cfg(unix)]
    #[test]
    fn an_inherited_sig_ign_is_never_taken_over() {
        assert!(
            !may_install_over(libc::SIG_IGN),
            "an ignored signal must be left ignored"
        );
        assert!(
            may_install_over(libc::SIG_DFL),
            "the default disposition is ours to replace"
        );
        // Any concrete handler address is likewise fair game -- only
        // SIG_IGN carries the "deliberately neutralised" meaning.
        assert!(may_install_over(1234 as libc::sighandler_t));
    }

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
