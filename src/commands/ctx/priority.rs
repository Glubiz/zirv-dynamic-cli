//! Scheduling posture, decided by role.
//!
//! On a machine busy with the workers zirv itself spawned -- each of them
//! running `cargo build`/`cargo test` across every core -- the operator's own
//! keystrokes used to compete with those builds on equal terms: the seat's
//! supervisor threads, the dashboard's UI thread, and a worker's cargo
//! grandchildren all ran at the normal priority class the OS hands every
//! process by default. `wrap`'s architecture was already right (stdin is
//! forwarded on a dedicated thread, pty output on another), so the missing
//! piece was never a queue or a buffer -- it was that nothing ever told the
//! scheduler which of those runnable threads a human is waiting on.
//!
//! The posture is a function of the session's ROLE, not of a knob:
//!
//! * [`Posture::Interactive`] -- a session an operator is sitting in front of
//!   (`zirv ctx wrap`, `zirv ctx chat`, the `zirv ctx dash` UI). It raises its
//!   own THREADS and never its process class: a `cargo build` the operator
//!   launches from that same seat is a child of the seat's own shell and would
//!   inherit a raised class, which is exactly the starvation this module
//!   exists to prevent -- the terminal must outrank the build it started, not
//!   share its rank.
//! * [`Posture::Worker`] -- a delegated session (`zirv ctx exec`, `zirv ctx
//!   loop`, `zirv ctx agent`, a script `agent:` step, a dashboard worker
//!   pane). It lowers the supervisor's process class one notch, and every
//!   child it spawns from that point on -- the harness binary, and in turn the
//!   cargo processes the harness runs -- inherits the lower class for free.
//!   Below-normal is not "starved": a below-normal process still gets every
//!   idle core on the machine. It only yields when something at normal
//!   priority (the operator's terminal) has work to do.
//!
//! There is deliberately NO escape hatch: no config key, no environment
//! variable. A posture that can be turned off is a posture that gets turned
//! off in exactly the situation it was written for.
//!
//! Everything here is best-effort and infallible by contract. A supervisor
//! must never fail, degrade, or even warn because the OS declined a
//! scheduling hint -- `wrap`'s "never worsen a session" rule applies to this
//! module in full, so there is no `unwrap`/`expect` and no error type: a
//! refused call simply leaves the session exactly as fast as it was before.

use super::prompt::PromptRole;

/// How a process, and the child it is about to spawn, should be scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    /// A human is typing into this session. Raise its own threads; leave the
    /// process class alone.
    Interactive,
    /// Delegated work. Lower the process class so its children inherit it.
    Worker,
}

/// The nice(2) increment a worker supervisor takes on unix. +5 is the same
/// order of magnitude as Windows' `BELOW_NORMAL_PRIORITY_CLASS`: clearly
/// behind an interactive process, still far ahead of true idle work.
#[cfg(unix)]
const WORKER_NICE: libc::c_int = 5;

/// The whole decision, as a pure function of the role the launch already
/// resolved for its prompt layers. Nothing else feeds it -- no config, no
/// terminal probe, no load average -- so a session's scheduling posture is
/// exactly as predictable as its prompt.
///
/// `SubOrchestrator` maps to `Worker`: it is a coordinator that was itself
/// *delegated* a scope (see [`PromptRole::SubOrchestrator`]), running in a
/// dashboard pane rather than at the seat, and it spawns and supervises real
/// build work. Only the seat -- the session an operator actually types into
/// -- gets the interactive posture, and it gets it exclusively; a second
/// "almost interactive" tier would just re-create the equal-footing problem
/// one level down.
pub fn posture_for(role: PromptRole) -> Posture {
    match role {
        PromptRole::Orchestrator => Posture::Interactive,
        PromptRole::SubOrchestrator | PromptRole::Worker => Posture::Worker,
    }
}

/// Applies `posture` to the CURRENT process, best-effort. Call it before the
/// supervised child is spawned: a Windows priority class and a unix nice
/// value are both inherited at creation time, so a call made after the spawn
/// reaches the supervisor and nothing it launched.
///
/// The worker half is called from a process's own DISPATCH, never from the
/// supervisor functions themselves: `ctx::dispatch` (for the `exec`, `loop`
/// and `agent` verbs, which is also how `zirv agent ...` arrives) and `main`
/// (for a script that has an `agent:` step). Every one of those supervisors
/// is driven in-process by unit tests, and a test binary must never lower a
/// process it does not own -- so a call inside `exec::run`/`run_loop::run`/
/// `agent::run`/`run_supervised` would silently leave the whole serial test
/// run at a below-normal class. The interactive half has no such hazard (it
/// only raises a thread) and is called from the launch itself,
/// `wrap::run_with`, where the role is already known.
///
/// Never fails the caller and never prints: see this module's contract.
pub fn apply_process(posture: Posture) {
    match posture {
        // Deliberately NOT a process-class change. See `Posture::Interactive`.
        Posture::Interactive => raise_current_thread(),
        Posture::Worker => lower_current_process(),
    }
}

/// The worker half of [`apply_process`], split out so the interactive path
/// cannot accidentally acquire a process-wide side effect.
fn lower_current_process() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
        };
        // SAFETY: no pointers. `GetCurrentProcess` returns the pseudo-handle
        // for this process, which needs no close and cannot be invalid; the
        // return value is ignored because a refused hint is not a failure.
        unsafe {
            SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: no pointers; `who = 0` names this process. Lowering a
        // process's own priority (raising its nice value) needs no privilege,
        // and a non-zero return is ignored for the same reason as above.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS as _, 0, WORKER_NICE);
        }
    }
}

/// Applies `posture` to an ALREADY-SPAWNED child by pid, best-effort.
///
/// The dashboard needs this and [`apply_process`] cannot serve it: a worker
/// pane's child is spawned by the dashboard process itself, which is the
/// operator's own UI and must stay at the normal class. Inheritance would
/// therefore hand a pane's agent -- and every cargo process under it -- the
/// UI's own priority, so the posture has to be stamped onto that one child
/// instead. Its own children inherit from it in the usual way.
///
/// `Interactive` is a no-op here on purpose: raising another process's class
/// is privileged on unix and wrong on Windows (see `Posture::Interactive`).
pub fn apply_to_child(pid: u32, posture: Posture) {
    if posture != Posture::Worker {
        return;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, OpenProcess, PROCESS_SET_INFORMATION, SetPriorityClass,
        };
        // SAFETY: no pointers. The handle is checked for null before use and
        // closed on the one path that obtains it; a pid that has already
        // exited, or one this token may not touch, simply yields null and the
        // pane keeps whatever priority it has.
        unsafe {
            let process = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
            if process.is_null() {
                return;
            }
            SetPriorityClass(process, BELOW_NORMAL_PRIORITY_CLASS);
            CloseHandle(process);
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: no pointers. Raising the nice value of another process
        // owned by the same user needs no privilege; a pid that has exited
        // returns an error that is deliberately ignored.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS as _, pid as libc::id_t, WORKER_NICE);
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
    }
}

/// Raises the CALLING thread one notch above normal, best-effort.
///
/// Called from the threads an operator's keystroke actually passes through --
/// `wrap`'s stdin forwarder and its pty-output reader, the dashboard's UI
/// thread and its pane readers -- so that on a machine saturated by
/// below-normal build work those threads are picked first when they become
/// runnable. Thread priority is per-thread and is NOT inherited by threads a
/// raised thread spawns, which is why this is called at the top of each of
/// those closures rather than once at startup.
///
/// A no-op off Windows: unix has no per-thread scheduling knob an
/// unprivileged process can turn up (nice is per-process there, and lowering
/// it back down needs privilege), so the interactive side of the posture is
/// Windows-only by nature -- which is also where the lag this fixes was
/// reported.
pub fn raise_current_thread() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        // SAFETY: no pointers. `GetCurrentThread` returns a pseudo-handle
        // that needs no close; the return value is ignored because a refused
        // hint is not a failure.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole decision surface. Note what is NOT here: no config, no env,
    /// no platform -- a role maps to a posture and that is the entire policy.
    #[test]
    fn only_the_seat_gets_the_interactive_posture() {
        assert_eq!(posture_for(PromptRole::Orchestrator), Posture::Interactive);
        assert_eq!(posture_for(PromptRole::SubOrchestrator), Posture::Worker);
        assert_eq!(posture_for(PromptRole::Worker), Posture::Worker);
    }

    /// The raise is what a keystroke-carrying thread actually gets, so assert
    /// the OS accepted it rather than just that the call returned. Done on a
    /// dedicated thread: thread priority is per-thread, so this leaves the
    /// test runner's own threads exactly as it found them.
    #[cfg(windows)]
    #[test]
    fn raising_the_current_thread_is_visible_to_the_os() {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        let observed = std::thread::spawn(|| {
            raise_current_thread();
            // SAFETY: no pointers; a pseudo-handle that needs no close.
            unsafe { GetThreadPriority(GetCurrentThread()) }
        })
        .join()
        .expect("the probe thread must not panic");
        assert_eq!(
            observed, THREAD_PRIORITY_ABOVE_NORMAL,
            "an interactive thread must actually outrank normal work"
        );
    }

    /// The interactive posture must never touch the PROCESS class -- a build
    /// started from the operator's own seat would inherit it. Verified by
    /// reading the class back after the call: `apply_process(Interactive)` is
    /// the one variant safe to run in-process from a test precisely because
    /// it has no process-wide effect.
    ///
    /// (`apply_process(Worker)` is deliberately never called in-process from
    /// a test: it would leave the whole test binary at a lowered class, with
    /// no unprivileged way to put it back.)
    #[cfg(windows)]
    #[test]
    fn the_interactive_posture_leaves_the_process_class_alone() {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetPriorityClass};
        // SAFETY: no pointers; a pseudo-handle that needs no close.
        let before = unsafe { GetPriorityClass(GetCurrentProcess()) };
        std::thread::spawn(|| apply_process(Posture::Interactive))
            .join()
            .expect("the probe thread must not panic");
        // SAFETY: as above.
        let after = unsafe { GetPriorityClass(GetCurrentProcess()) };
        assert_eq!(
            before, after,
            "the seat's own posture must not reach the process class"
        );
    }
}
