//! The vendor gate: a script that runs in place of the vendor binary and does
//! nothing until Octet has registered it.
//!
//! [`write_gate`] writes an owner-only `sh` script, `<attempt>/gate`, that
//! exports the attempt's marker, appends its own PID to `<attempt>/processes`
//! and then waits for `<attempt>/go`. It polls every 50 ms and exits 125,
//! having run nothing, when Octet is gone or after 120 s. Once `go` exists it
//! `exec`s the real binary, keeping its PID. Octet reads the PIDs with
//! [`read_processes`], makes their identities durable, and only then calls
//! [`release`]. A later launch of the gate in the same attempt finds `go`
//! already there: it registers its PID and runs at once.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use octet_store::sync_directory;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

/// The file the gate appends its PID to.
const PROCESSES: &str = "processes";

/// The file whose existence releases the gate.
const GO: &str = "go";

/// How many 50 ms polls the gate makes before it gives up: 120 s.
const POLLS: u32 = 2400;

/// Why the gate could not be written or released.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// A path the script would embed is relative, so it would name another file
    /// from another directory.
    #[error("{} is not an absolute path", .0.display())]
    NotAbsolute(PathBuf),
    /// A file could not be written or synced.
    #[error("gate I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// `value` as one word of `sh`: single-quoted, each embedded quote written as
/// `'\''`.
fn quote(value: &OsStr) -> Vec<u8> {
    let mut quoted = vec![b'\''];
    for &byte in value.as_bytes() {
        if byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(byte);
        }
    }
    quoted.push(b'\'');
    quoted
}

/// The text of the gate script.
fn script(attempt_dir: &Path, real_binary: &Path, marker: &str, octet_pid: u32) -> Vec<u8> {
    // Paths need not be UTF-8, so the bytes are joined, not formatted.
    let go = quote(attempt_dir.join(GO).as_os_str());
    let processes = quote(attempt_dir.join(PROCESSES).as_os_str());
    let mut text = Vec::new();
    let mut push = |parts: &[&[u8]]| {
        for part in parts {
            text.extend_from_slice(part);
        }
        text.push(b'\n');
    };
    push(&[b"#!/bin/sh"]);
    push(&[b"export OCTET_FLEET_ATTEMPT=", &quote(OsStr::new(marker))]);
    push(&[b"echo $$ >> ", &processes, b" || exit 125"]);
    push(&[b"if [ ! -e ", &go, b" ]; then"]);
    push(&[b"  n=0"]);
    push(&[
        b"  while [ $n -lt ",
        POLLS.to_string().as_bytes(),
        b" ]; do",
    ]);
    push(&[b"    [ -e ", &go, b" ] && break"]);
    push(&[
        b"    kill -0 ",
        octet_pid.to_string().as_bytes(),
        b" 2>/dev/null || exit 125",
    ]);
    push(&[b"    sleep 0.05"]);
    push(&[b"    n=$((n + 1))"]);
    push(&[b"  done"]);
    push(&[b"  [ -e ", &go, b" ] || exit 125"]);
    push(&[b"fi"]);
    push(&[b"exec ", &quote(real_binary.as_os_str()), b" \"$@\""]);
    text
}

/// Writes `<attempt_dir>/gate`, owner-only and executable, and returns its path.
/// The file is complete and synced, in its directory too, when this returns.
///
/// `marker` is the value of `OCTET_FLEET_ATTEMPT` (`<run>/<attempt>`) the gate
/// exports; `octet_pid` is the process whose death releases the gate to give up.
///
/// # Errors
///
/// [`GateError::NotAbsolute`] when `attempt_dir` or `real_binary` is relative;
/// [`GateError::Io`] when the file cannot be written.
pub async fn write_gate(
    attempt_dir: &Path,
    real_binary: &Path,
    marker: &str,
    octet_pid: u32,
) -> Result<PathBuf, GateError> {
    for path in [attempt_dir, real_binary] {
        if !path.is_absolute() {
            return Err(GateError::NotAbsolute(path.to_path_buf()));
        }
    }
    let text = script(attempt_dir, real_binary, marker, octet_pid);
    let path = attempt_dir.join("gate");
    let tmp = attempt_dir.join("gate.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o700)
        .open(&tmp)
        .await?;
    file.write_all(&text).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, &path).await?;
    sync_directory(attempt_dir).await?;
    Ok(path)
}

/// The PIDs the gate has registered in `<attempt_dir>/processes`, in order.
/// Blank and unparseable lines are skipped; no file is no PIDs.
///
/// # Errors
///
/// [`GateError::Io`] when the file exists but cannot be read: an unreadable
/// registry must never pass for an empty one, or a gate would be released
/// unregistered.
pub async fn read_processes(attempt_dir: &Path) -> Result<Vec<u32>, GateError> {
    let bytes = match tokio::fs::read(attempt_dir.join(PROCESSES)).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect())
}

/// Lets the gate through: creates `<attempt_dir>/go` and makes it durable.
/// Call it only after the registered identities are.
///
/// # Errors
///
/// [`GateError::Io`] when the file cannot be created or synced.
pub async fn release(attempt_dir: &Path) -> Result<(), GateError> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(attempt_dir.join(GO))
        .await?;
    file.sync_all().await?;
    drop(file);
    sync_directory(attempt_dir).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use octet_testkit::{QUICK, TempDir, wait_until, write_script};

    use super::*;

    /// A target that records its arguments and the marker it inherited.
    fn target(dir: &Path) -> PathBuf {
        write_script(
            &dir.join("bin"),
            "target",
            &format!(
                "echo \"$OCTET_FLEET_ATTEMPT $*\" >> '{}'",
                dir.join("ran").display()
            ),
        )
    }

    fn attempt_dir(dir: &TempDir) -> PathBuf {
        let attempt = dir.path().join("attempt");
        fs::create_dir_all(&attempt).expect("attempt dir");
        attempt
    }

    fn launch(gate: &Path, args: &[&str]) -> std::process::Child {
        Command::new(gate)
            .args(args)
            .stdin(Stdio::null())
            .spawn()
            .expect("the gate starts")
    }

    #[test]
    fn quoting_survives_quotes_and_spaces() {
        let tricky = "/tmp/it's a dir/'x'";
        let quoted = quote(OsStr::new(tricky));
        assert_eq!(quoted, b"'/tmp/it'\\''s a dir/'\\''x'\\'''");
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {}", String::from_utf8_lossy(&quoted)))
            .output()
            .expect("sh runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), tricky);
    }

    #[tokio::test]
    async fn a_relative_binary_or_directory_is_refused() {
        let dir = TempDir::new("octet-fleet-gate");
        let attempt = attempt_dir(&dir);
        let relative = write_gate(&attempt, Path::new("claude"), "r/0", 1).await;
        assert!(matches!(relative, Err(GateError::NotAbsolute(_))));
        let relative = write_gate(Path::new("attempt"), Path::new("/bin/sh"), "r/0", 1).await;
        assert!(matches!(relative, Err(GateError::NotAbsolute(_))));
        assert!(!attempt.join("gate").exists());
    }

    #[tokio::test]
    async fn the_gate_is_owner_only_and_executable() {
        let dir = TempDir::new("octet-fleet-gate");
        let attempt = attempt_dir(&dir);
        let gate = write_gate(&attempt, &target(dir.path()), "r/0", std::process::id())
            .await
            .expect("written");
        assert_eq!(gate, attempt.join("gate"));
        let mode = fs::metadata(&gate).expect("gate").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(!attempt.join("gate.tmp").exists());
    }

    #[tokio::test]
    async fn the_gate_waits_for_go() {
        let dir = TempDir::new("octet-fleet-gate");
        let attempt = attempt_dir(&dir);
        let real = target(dir.path());
        let gate = write_gate(&attempt, &real, "run's/0", std::process::id())
            .await
            .expect("written");
        let mut child = launch(&gate, &["a", "b c"]);
        // It registers itself first.
        assert!(wait_until(QUICK, || !read_processes_now(&attempt).is_empty()));
        assert_eq!(
            read_processes(&attempt).await.expect("read"),
            vec![child.id()]
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !dir.path().join("ran").exists(),
            "nothing ran before release"
        );
        assert!(
            child.try_wait().expect("poll").is_none(),
            "it is still waiting"
        );
        release(&attempt).await.expect("released");
        assert!(child.wait().expect("waits").success());
        let ran = fs::read_to_string(dir.path().join("ran")).expect("ran");
        assert_eq!(ran, "run's/0 a b c\n");
    }

    /// `read_processes` for a poll that is not async.
    fn read_processes_now(attempt: &Path) -> Vec<u32> {
        fs::read_to_string(attempt.join(PROCESSES))
            .map(|text| text.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn the_gate_gives_up_when_octet_is_gone() {
        let dir = TempDir::new("octet-fleet-gate");
        let attempt = attempt_dir(&dir);
        let mut gone = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("a short-lived child");
        let dead = gone.id();
        gone.wait().expect("reaped");
        let gate = write_gate(&attempt, &target(dir.path()), "r/0", dead)
            .await
            .expect("written");
        let started = Instant::now();
        let mut child = launch(&gate, &[]);
        let status = child.wait().expect("waits");
        assert_eq!(status.code(), Some(125));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it gave up promptly"
        );
        assert!(!dir.path().join("ran").exists());
        assert_eq!(
            read_processes(&attempt).await.expect("read"),
            vec![child.id()]
        );
    }

    #[tokio::test]
    async fn later_launches_pass_at_once_and_register() {
        let dir = TempDir::new("octet-fleet-gate");
        let attempt = attempt_dir(&dir);
        let gate = write_gate(&attempt, &target(dir.path()), "r/0", std::process::id())
            .await
            .expect("written");
        release(&attempt).await.expect("released");
        let mut first = launch(&gate, &["one"]);
        assert!(first.wait().expect("waits").success());
        let mut second = launch(&gate, &["two"]);
        assert!(second.wait().expect("waits").success());
        assert_eq!(
            read_processes(&attempt).await.expect("read"),
            vec![first.id(), second.id()]
        );
        let ran = fs::read_to_string(dir.path().join("ran")).expect("ran");
        assert_eq!(ran, "r/0 one\nr/0 two\n");
    }

    #[tokio::test]
    async fn processes_skips_blank_and_garbage_lines() {
        let dir = TempDir::new("octet-fleet-gate");
        let attempt = attempt_dir(&dir);
        assert!(read_processes(&attempt).await.expect("read").is_empty());
        fs::write(
            attempt.join(PROCESSES),
            "12\n\nx7\n 34 \n-1\n99999999999\n56",
        )
        .expect("write");
        assert_eq!(
            read_processes(&attempt).await.expect("read"),
            vec![12, 34, 56]
        );
    }

    #[tokio::test]
    async fn release_is_owner_only_and_repeatable() {
        let dir = TempDir::new("octet-fleet-gate");
        let attempt = attempt_dir(&dir);
        release(&attempt).await.expect("first");
        release(&attempt).await.expect("again");
        let mode = fs::metadata(attempt.join(GO))
            .expect("go")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn an_unreadable_registry_is_an_error_not_no_pids() {
        let dir = TempDir::new("octet-fleet-gate");
        let attempt = attempt_dir(&dir);
        // A directory where the file belongs: reading it fails, and not with NotFound.
        fs::create_dir(attempt.join(PROCESSES)).expect("directory");
        assert!(matches!(
            read_processes(&attempt).await,
            Err(GateError::Io(_))
        ));
    }

    #[tokio::test]
    async fn the_gate_refuses_to_run_when_it_cannot_register() {
        for released in [false, true] {
            let dir = TempDir::new("octet-fleet-gate");
            let attempt = attempt_dir(&dir);
            let gate = write_gate(&attempt, &target(dir.path()), "r/0", std::process::id())
                .await
                .expect("written");
            // The append fails: a directory is where the registry belongs.
            fs::create_dir(attempt.join(PROCESSES)).expect("directory");
            if released {
                release(&attempt).await.expect("released");
            }
            let started = Instant::now();
            let mut child = launch(&gate, &["x"]);
            assert_eq!(
                child.wait().expect("waits").code(),
                Some(125),
                "released: {released}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "it did not wait"
            );
            assert!(!dir.path().join("ran").exists(), "released: {released}");
        }
    }
}
