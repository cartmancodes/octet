//! Signalling one process by ID, for callers that have verified whose it is.
//!
//! Every function refuses PID 0 and any PID that does not fit `pid_t` without calling the
//! kernel: `kill(0, …)` signals the caller's own process group, and a negative `pid_t` names a
//! group or, as `-1`, every process the caller may signal.

/// A PID that names exactly one process, as the kernel's type.
fn single(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)
}

/// Whether a process with this ID exists, whoever owns it. A zombie (exited, not yet reaped)
/// still exists. `false` for PID 0 and for any ID above `i32::MAX`, which are never looked up.
pub fn alive(pid: u32) -> bool {
    let Some(pid) = single(pid) else {
        return false;
    };
    // SAFETY: signal 0 performs only the existence and permission check.
    (unsafe { libc::kill(pid, 0) } == 0)
        // EPERM: it exists but is not ours to signal.
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Asks the process to stop (SIGTERM); whether the signal was sent. `false` for PID 0, for any
/// ID above `i32::MAX`, and when the process is gone or not ours.
pub fn terminate(pid: u32) -> bool {
    send(pid, libc::SIGTERM)
}

/// Kills the process (SIGKILL); whether the signal was sent. `false` for PID 0, for any ID
/// above `i32::MAX`, and when the process is gone or not ours.
pub fn kill(pid: u32) -> bool {
    send(pid, libc::SIGKILL)
}

fn send(pid: u32, signal: libc::c_int) -> bool {
    let Some(pid) = single(pid) else {
        return false;
    };
    // SAFETY: kill only sends a signal, to the one positive PID checked above.
    unsafe { libc::kill(pid, signal) == 0 }
}

#[cfg(test)]
mod tests {
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    fn sleeper() -> Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep starts")
    }

    fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "the child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn pid_zero_is_never_signalled() {
        assert!(!alive(0));
        assert!(!terminate(0));
        assert!(!kill(0));
    }

    #[test]
    fn a_pid_past_pid_t_is_never_looked_up() {
        for pid in [u32::try_from(i32::MAX).expect("positive") + 1, u32::MAX] {
            assert!(!alive(pid));
            assert!(!terminate(pid));
            assert!(!kill(pid));
        }
    }

    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id()));
    }

    #[test]
    fn terminate_stops_a_child() {
        let mut child = sleeper();
        assert!(alive(child.id()));
        assert!(terminate(child.id()));
        let status = wait_exit(&mut child);
        assert!(!status.success());
        assert!(!alive(child.id()), "reaped, so gone");
    }

    #[test]
    fn kill_stops_a_child() {
        let mut child = sleeper();
        assert!(kill(child.id()));
        wait_exit(&mut child);
        assert!(!kill(child.id()), "nothing left to signal");
    }
}
