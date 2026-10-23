//! A child that leads its own process group, and the end of that whole group.
use std::time::Duration;
use tokio::process::Child;

/// How long a killed group leader may take to die before [`GroupLeader::kill_group`] kills
/// its group again regardless. It blocks the calling thread, but a process dies of SIGKILL in a
/// moment.
const LEADER_DEATH: Duration = Duration::from_secs(1);

/// How often a wait for the leader to die looks.
const LOOK: Duration = Duration::from_millis(1);

/// A child spawned to lead its own process group (`process_group(0)` or `setsid`), owned here so
/// that only its owner can reap it (tokio reaps a child only when it is waited for or after it
/// drops). Until it is reaped, its pid, the group's id, cannot be reused, so a signal to the
/// group reaches this group and no other. Once it is reaped, nothing here signals the group
/// again: its id may then name another. Dropped, it kills its whole group first (see
/// [`GroupLeader::kill_group`]).
#[derive(Debug)]
pub struct GroupLeader(Child);

/// Where the leader is, as seen without reaping it.
#[derive(Debug, PartialEq, Eq)]
enum Leader {
    /// It has not exited.
    Running,
    /// It has exited but is unreaped: its pid and the group's id are still its own.
    Exited,
    /// It has been reaped (or is no child of this process): its pid may be another's.
    Reaped,
}

impl GroupLeader {
    /// Takes `child`, which must lead its own process group.
    pub fn new(child: Child) -> Self {
        Self(child)
    }

    /// The child, for its pipes and to wait for it. Waiting reaps it, after which its group is
    /// no longer signalled.
    pub fn child(&mut self) -> &mut Child {
        &mut self.0
    }

    /// Asks the whole group to stop (SIGTERM), unless the leader has been reaped; whether it
    /// was asked.
    pub fn term_group(&self) -> bool {
        self.signal_group(libc::SIGTERM)
    }

    /// Sends `signal` to the leader's whole group, unless the leader has been reaped; whether
    /// it was sent.
    fn signal_group(&self, signal: libc::c_int) -> bool {
        let Some(group) = self.pid() else {
            return false;
        };
        // The leader cannot be reaped between this look and the signal: only its owner reaps
        // it, and the owner is here.
        if self.leader() == Leader::Reaped {
            return false;
        }
        // SAFETY: kill only sends a signal, to the group the unreaped leader's pid still names.
        unsafe { libc::kill(-group, signal) == 0 }
    }

    /// SIGKILLs the whole group. A child a member is forking as the signal goes out is not in
    /// the group yet, and macOS does not signal it: the member dies and the child lives on. So
    /// once the leader has died (or a second has passed) the group is killed again,
    /// which gets the rest when the leader was the one forking: a dead process forks no more.
    /// The leader stays unreaped throughout. A leader already reaped is not signalled at all.
    pub fn kill_group(&self) {
        if !self.signal_group(libc::SIGKILL) {
            return;
        }
        let deadline = std::time::Instant::now() + LEADER_DEATH;
        while self.leader() == Leader::Running && std::time::Instant::now() < deadline {
            std::thread::sleep(LOOK);
        }
        self.signal_group(libc::SIGKILL);
    }

    /// Waits, at most `limit`, for the leader to exit, without reaping it; whether it has.
    pub async fn exited_within(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if self.leader() != Leader::Running {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The leader's pid, the group's id; none once tokio has reaped it.
    fn pid(&self) -> Option<libc::pid_t> {
        self.0
            .id()
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
            // Never every process, nor init's group.
            .filter(|pid| *pid > 1)
    }

    /// Where the leader is, without reaping it.
    fn leader(&self) -> Leader {
        let Some(id) = self.pid().and_then(|pid| libc::id_t::try_from(pid).ok()) else {
            return Leader::Reaped;
        };
        loop {
            // SAFETY: siginfo_t is plain C data, for which all zeroes is valid.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid writes only into `info`; WNOWAIT leaves the child unreaped, and
            // WNOHANG returns at once.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    id,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if result != 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Leader::Reaped;
            }
            // SAFETY: waitid filled `info`; its pid stays zero while the child has not exited.
            return if unsafe { info.si_pid() } == 0 {
                Leader::Running
            } else {
                Leader::Exited
            };
        }
    }
}

impl Drop for GroupLeader {
    fn drop(&mut self) {
        self.kill_group();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt as _;

    /// Whether the process `pid` is alive (or a zombie not yet reaped).
    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Starts `script` with `sh`, leading its own group, and reads the pid its first line names.
    async fn started(script: &str) -> (Child, libc::pid_t) {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", script])
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("sh");
        let mut line = String::new();
        let stdout = child.stdout.take().expect("stdout");
        tokio::io::BufReader::new(stdout)
            .read_line(&mut line)
            .await
            .expect("a line");
        (child, line.trim().parse().expect("a pid"))
    }

    #[tokio::test]
    async fn a_reaped_leaders_group_is_not_signalled() {
        // Once the leader is reaped its pid, the group's id, may name another group: one the
        // kill must not reach. Here the old group lives on in a `sleep`, and is left alone.
        let (child, sleep) = started("sleep 30 & echo $!").await;
        let mut leader = GroupLeader::new(child);
        leader.child().wait().await.expect("sh exits");
        leader.kill_group();
        drop(leader);
        // A signalled sleep dies and is reaped by init within a moment.
        let left = !octet_testkit::wait_until(Duration::from_secs(1), || !alive(sleep));
        // SAFETY: signals only the sleep this test started.
        unsafe { libc::kill(sleep, libc::SIGKILL) };
        assert!(left, "a reaped leader's group was signalled");
    }

    #[tokio::test]
    async fn an_unreaped_leaders_whole_group_is_killed_as_it_drops() {
        let (child, sleep) = started("sleep 30 & echo $!; wait").await;
        drop(GroupLeader::new(child));
        // The sleep, orphaned, is reaped by init once it dies.
        let gone = octet_testkit::wait_until(octet_testkit::QUICK, || !alive(sleep));
        // SAFETY: signals only the sleep this test started.
        unsafe { libc::kill(sleep, libc::SIGKILL) };
        assert!(gone, "the group outlived its leader's drop");
    }
}
