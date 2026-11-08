//! Cooperative cancellation of a backup run (SIGTERM / SIGINT).
//!
//! The handler only sets a flag; the run checks it between phases, while
//! waiting for the PBS and while the client runs (it is then stopped).

use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);

pub fn requested() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

pub fn request() {
    CANCELLED.store(true, Ordering::SeqCst);
}

#[cfg(test)]
pub fn reset() {
    CANCELLED.store(false, Ordering::SeqCst);
}

extern "C" fn on_signal(_: nix::libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}

/// Install handlers for SIGTERM, SIGINT and SIGHUP. Handlers (not SIG_IGN)
/// are reset to the default in child processes on exec.
pub fn install_handler() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(
        SigHandler::Handler(on_signal),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        // SAFETY: the handler only stores to an atomic.
        unsafe {
            let _ = sigaction(sig, &action);
        }
    }
}
