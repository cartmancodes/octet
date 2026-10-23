//! Test support shared by the workspace: the fake vendor binary and
//! self-cleaning temporary directories.
pub mod scenario;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// How long "finishes at once" may take on a machine busy with the suite.
pub const QUICK: Duration = Duration::from_secs(5);

/// Polls `condition` every 10 ms until it holds or `timeout` passes; true if
/// it held.
pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Builds `protocol-child` for the profile the calling test runs in and
/// returns its path.
pub fn protocol_child() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| testkit_bin("protocol-child")).clone()
}

/// Builds `detached-sleep`, which leaves its process group (`setsid`) and
/// sleeps for the seconds given as its argument.
pub fn detached_sleep() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| testkit_bin("detached-sleep")).clone()
}

/// Builds one of this crate's binaries for the profile the calling test runs
/// in, and returns the copy tests run. Release and custom target directories
/// get their own.
///
/// Test processes sharing a target directory each call this as they start.
/// Cargo replaces its own copy (`<profile>/<name>`) on every build, fresh or
/// not, removing it first, so a test running that one could find it missing
/// or half written. Tests run a copy of it in `<profile>/octet-testkit/`
/// instead, replaced only when cargo's has changed (its length or time), and
/// then by a rename, so a test sees the old binary or the new one, whole.
/// Builds and copies take turns, under a lock in that directory.
fn testkit_bin(name: &str) -> PathBuf {
    // Test executables live in <target>/<profile>/deps/.
    let profile_dir = std::env::current_exe()
        .expect("test executable path")
        .parent()
        .and_then(Path::parent)
        .expect("test executable inside a profile directory")
        .to_path_buf();
    let target_dir = profile_dir.parent().expect("target directory");
    let profile = match profile_dir.file_name().and_then(|name| name.to_str()) {
        Some("debug") | None => "dev",
        Some(other) => other,
    };
    let kept_dir = profile_dir.join("octet-testkit");
    std::fs::create_dir_all(&kept_dir).expect("the test binaries' directory");
    let lock = std::fs::File::create(kept_dir.join(".lock")).expect("the build lock");
    lock.lock().expect("the build lock");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new(&cargo)
        .args(["build", "--quiet", "--locked", "-p", "octet-testkit"])
        .args(["--bin", name, "--profile", profile])
        .arg("--target-dir")
        .arg(target_dir)
        .current_dir(&workspace)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "run {} in {} to build {name}: {error}",
                Path::new(&cargo).display(),
                workspace.display()
            )
        });
    assert!(status.success(), "building {name} failed");
    let built = profile_dir.join(name);
    let kept = kept_dir.join(name);
    if changed(&built, &kept) {
        let temporary = kept_dir.join(format!(".{name}.{}", std::process::id()));
        std::fs::copy(&built, &temporary).expect("a copy of the built binary");
        let time = std::fs::metadata(&built)
            .and_then(|meta| meta.modified())
            .expect("the built binary's time");
        std::fs::File::options()
            .write(true)
            .open(&temporary)
            .and_then(|copy| copy.set_modified(time))
            .expect("the copy's time");
        std::fs::rename(&temporary, &kept).expect("the copy in place");
    }
    kept
}

/// Whether `copy` differs from `built` in length or modification time, or
/// is missing.
fn changed(built: &Path, copy: &Path) -> bool {
    let look = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|meta| Ok((meta.len(), meta.modified()?)))
            .ok()
    };
    look(copy).is_none() || look(copy) != look(built)
}

/// Writes an executable shell script `dir/name` (creating `dir`): a
/// `#!/bin/sh` line, then `body`. Returns its path.
///
/// # Panics
///
/// If the script cannot be written.
pub fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).expect("script directory");
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    path
}

/// `PATH` with `dir` first, so programs written there shadow real ones.
pub fn path_with(dir: &Path) -> String {
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Waits for `child` to exit, collecting its output, at most `limit`.
///
/// # Panics
///
/// If it has not exited within `limit` (it is killed first, so a hung
/// process never outlives the test), or cannot be waited for.
pub fn wait_child(child: std::process::Child, limit: Duration) -> std::process::Output {
    let pid = libc::pid_t::try_from(child.id()).expect("pid fits pid_t");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(child.wait_with_output()));
    if let Ok(output) = rx.recv_timeout(limit) {
        output.expect("wait for the process")
    } else {
        // SAFETY: kill only sends a signal. The child is still unreaped
        // (the waiting thread has not returned), unless it exited in this
        // very instant; a test helper accepts that tiny window.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("the process did not exit in time");
    }
}

/// Runs `body` as if on a full disk: in a child process (this test binary,
/// running only the test named `test`, its full path) whose files cannot
/// grow past `bytes`. A write past the limit fails (EFBIG) instead of
/// killing the process.
///
/// # Panics
///
/// In the parent, if `body` failed in the child or the child cannot run;
/// in the child, if the limit cannot be set.
pub fn with_file_limit(test: &str, bytes: u64, body: impl FnOnce()) {
    const CHILD: &str = "OCTET_TESTKIT_FILE_LIMIT";
    if std::env::var_os(CHILD).is_some_and(|name| name == test) {
        let limit = libc::rlimit {
            rlim_cur: bytes,
            rlim_max: bytes,
        };
        // SAFETY: ignoring SIGXFSZ and lowering this process's own limit
        // touch no memory; the child runs nothing else.
        unsafe {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limit), 0);
        }
        body();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args([test, "--exact", "--nocapture"])
        .env(CHILD, test)
        .output()
        .expect("run the test in a child");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Each line `output` prints, read on a thread of its own.
pub fn line_reader(
    output: impl std::io::Read + Send + 'static,
) -> std::sync::mpsc::Receiver<String> {
    use std::io::BufRead;
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(output)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

/// The journal record format; `octet_store::FORMAT` is pinned to it by a
/// store test.
pub const JOURNAL_FORMAT: &str = "octet-preview-1";

/// Writes a journal named for `stamp` in `dir` (created if missing) from
/// `records` (`(type, data)` pairs, numbered in order), then `tail`
/// unterminated, as a crash might leave it. Returns its path.
///
/// # Panics
///
/// If the directory or file cannot be written.
pub fn write_journal(
    dir: &Path,
    stamp: u64,
    records: &[(&str, serde_json::Value)],
    tail: &str,
) -> PathBuf {
    std::fs::create_dir_all(dir).expect("journal directory");
    let path = dir.join(format!("session-{stamp}-1.jsonl"));
    let mut text = String::new();
    for (seq, (kind, data)) in records.iter().enumerate() {
        let record =
            serde_json::json!({"format": JOURNAL_FORMAT, "seq": seq, "type": kind, "data": data});
        text.push_str(&record.to_string());
        text.push('\n');
    }
    text.push_str(tail);
    std::fs::write(&path, text).expect("journal file");
    path
}

/// A unique path under the system temp dir, removed when dropped, so a
/// failing test does not leave it behind. The path is not created.
#[derive(Debug)]
pub struct TempDir(PathBuf);
impl TempDir {
    /// A fresh path named after `prefix`, unique within this test run.
    pub fn new(prefix: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        Self(path)
    }
    /// The path; the directory exists only once a test creates it.
    pub fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        // Some tests put a file at the path to make writes fail.
        if std::fs::remove_dir_all(&self.0).is_err() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wait_child_kills_on_timeout() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep starts");
        let pid = libc::pid_t::try_from(child.id()).expect("pid fits pid_t");
        let waited = std::panic::catch_unwind(|| wait_child(child, Duration::from_millis(100)));
        assert!(waited.is_err(), "a hung process fails the test");
        // SAFETY: signal 0 only checks that the process exists.
        let gone = wait_until(QUICK, || unsafe { libc::kill(pid, 0) } != 0);
        assert!(gone, "the hung process was left running");
    }
    #[test]
    fn wait_until_reports_whether_the_condition_held() {
        let mut calls = 0;
        assert!(wait_until(QUICK, || {
            calls += 1;
            calls == 3
        }));
        assert!(!wait_until(Duration::from_millis(30), || false));
    }
    #[test]
    fn a_build_beside_a_running_test_leaves_its_binary_in_place() {
        // Test processes sharing a target directory each build the fake vendor as they start;
        // one that builds must not take away, or rewrite, the binary another is running.
        use std::os::unix::fs::MetadataExt as _;
        use std::sync::atomic::AtomicBool;
        let path = protocol_child();
        let inode = std::fs::metadata(&path).expect("the binary").ino();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let watcher = {
            let (path, stop) = (path.clone(), std::sync::Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut seen = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let now = std::fs::metadata(&path).map(|meta| meta.ino()).ok();
                    if now != Some(inode) && seen.last() != Some(&now) {
                        seen.push(now);
                    }
                }
                seen
            })
        };
        let again = testkit_bin("protocol-child");
        stop.store(true, Ordering::Relaxed);
        let seen = watcher.join().expect("the watcher");
        assert_eq!(again, path);
        assert!(
            seen.is_empty(),
            "the binary was replaced or missing during a build: {seen:?}"
        );
    }
    #[test]
    fn temp_dirs_are_unique_and_removed() {
        let first = TempDir::new("octet-testkit");
        let second = TempDir::new("octet-testkit");
        assert_ne!(first.path(), second.path());
        std::fs::create_dir_all(first.path()).unwrap();
        let path = first.path().to_path_buf();
        drop(first);
        assert!(!path.exists());
    }
}
