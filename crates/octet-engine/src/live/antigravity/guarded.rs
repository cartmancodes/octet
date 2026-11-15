//! How Octet keeps a file of its own under its data directory for Antigravity:
//! a folder it makes owner-only after checking that it is not inside
//! Antigravity's own folder, and a file it writes there owner-only, under a
//! temporary name, then renames into place. Antigravity's own folder
//! (`~/.gemini`) is never written.
use std::{
    io::{ErrorKind, Write as _},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Numbers the temporary files of this process, so no two calls share a name.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A temporary name that is taken (by a file a crashed run left, perhaps under
/// the same process id) is skipped, this many times at most.
const TEMP_TRIES: usize = 16;

/// Why a folder or file could not be made.
#[derive(Debug)]
pub(super) enum Failure {
    /// The place is inside Antigravity's own folder; this is the refusal.
    Refused(String),
    /// Something at `path` failed, for `reason`.
    Failed {
        /// Where.
        path: PathBuf,
        /// Why, as the system said it.
        reason: String,
    },
}

impl Failure {
    fn failed(path: &Path, reason: impl std::fmt::Display) -> Self {
        Self::Failed {
            path: path.to_owned(),
            reason: reason.to_string(),
        }
    }

    /// The text to fail with: the refusal as it is, or `failed` for the
    /// path and reason of any other failure.
    pub(super) fn into_text(self, failed: impl FnOnce(&Path, &str) -> String) -> String {
        match self {
            Self::Refused(text) => text,
            Self::Failed { path, reason } => failed(&path, &reason),
        }
    }
}

/// `path` with its links resolved. A path that does not exist yet is
/// resolved through its deepest folder that does, the rest appended
/// unchanged.
fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        match at.canonicalize() {
            Ok(mut resolved) => {
                resolved.extend(rest.iter().rev());
                return Ok(resolved);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let (Some(name), Some(parent)) = (at.file_name(), at.parent()) else {
            return Err(ErrorKind::NotFound.into());
        };
        rest.push(name);
        at = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
    }
}

/// `home` resolved as far as it exists, so a folder inside one that is not
/// there yet (or is a dangling link) is still refused.
fn resolve_home(home: Option<&Path>) -> Result<Option<PathBuf>, Failure> {
    home.map(|home| resolve(home).map_err(|error| Failure::failed(home, error)))
        .transpose()
}

/// `folder` resolved, unless that is one of the resolved vendor homes `homes`
/// or is under one; the refusal names `kept`, what the folder would hold.
/// Nothing is created.
fn keep_out_of(folder: &Path, homes: &[PathBuf], kept: &str) -> Result<PathBuf, Failure> {
    let resolved = resolve(folder).map_err(|error| Failure::failed(folder, error))?;
    if homes.iter().any(|home| resolved.starts_with(home)) {
        return Err(Failure::Refused(format!(
            "Octet will not keep {kept} inside Antigravity's own folder ({}); \
             choose another --journal-dir",
            resolved.display()
        )));
    }
    Ok(resolved)
}

/// Makes `<data_dir>/<below>` (owner-only where it is new) and returns it,
/// unless it, or `data_dir`, is inside `vendor_home` (by a link or by
/// `--journal-dir`) or is not where `below` says once links are followed.
/// Everything is checked before anything is created, and again after. A
/// refusal names `kept`, what the folder is for ("its Antigravity
/// settings").
pub(super) fn folder(
    data_dir: &Path,
    below: &Path,
    vendor_home: Option<&Path>,
    kept: &str,
) -> Result<PathBuf, Failure> {
    let dir = data_dir.join(below);
    let before: Vec<PathBuf> = resolve_home(vendor_home)?.into_iter().collect();
    keep_out_of(data_dir, &before, kept)?;
    keep_out_of(&dir, &before, kept)?;
    // Only folders Octet creates are owner-only.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| Failure::failed(&dir, error))?;
    // Making the folder can bring the vendor home into being (a link that
    // pointed nowhere, a name that differs only in letter case), so it is
    // resolved again and the folder must be outside both.
    let mut homes = before;
    homes.extend(resolve_home(vendor_home)?);
    let real_data = keep_out_of(data_dir, &homes, kept)?;
    let real_dir = keep_out_of(&dir, &homes, kept)?;
    // The folder must be the one meant, not a link made since the first
    // check, nor a link from one folder to another.
    if real_dir != real_data.join(below) {
        return Err(Failure::failed(
            &dir,
            "the folder is a link to another place",
        ));
    }
    Ok(dir)
}

/// Writes `bytes` to `<dir>/<name>`, owner-only, through a temporary file named
/// for this call alone and renamed into place.
pub(super) fn write_private(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    write_private_with(dir, name, bytes, || {
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    })
}

/// [`write_private`] with the temporary names' numbers from `next`.
fn write_private_with(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    mut next: impl FnMut() -> u64,
) -> std::io::Result<()> {
    for _ in 0..TEMP_TRIES {
        let temp = dir.join(format!("{name}.{}.{}.tmp", std::process::id(), next()));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => file,
            // Not ours: left by a run that stopped, so it is left alone.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        // A file this call made and could not fill or place is removed.
        return file
            .write_all(bytes)
            .and_then(|()| std::fs::rename(&temp, dir.join(name)))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&temp);
            });
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "no unused temporary file name",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::TempDir;
    use std::os::unix::fs::symlink;

    #[test]
    fn a_vendor_home_that_the_new_folder_brings_into_being_is_refused() {
        let root = TempDir::new("octet-agy-guarded-dangling");
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        // `.gemini` points at a place that does not exist yet.
        let home = root.path().join("home/.gemini");
        symlink(root.path().join("elsewhere"), &home).unwrap();
        let data = root.path().join("elsewhere/octet");
        let kept = "its Antigravity settings";
        let failure = folder(&data, Path::new("antigravity"), Some(&home), kept).unwrap_err();
        let Failure::Refused(text) = failure else {
            panic!("expected the refusal, got {failure:?}");
        };
        assert!(
            text.starts_with("Octet will not keep its Antigravity settings inside"),
            "{text}"
        );
        // Folders made before the second look are acceptable; a file is not.
        let mut found = Vec::new();
        let mut pending = vec![root.path().to_owned()];
        while let Some(at) = pending.pop() {
            for entry in std::fs::read_dir(&at).unwrap().flatten() {
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if kind.is_file() {
                    found.push(entry.path());
                }
            }
        }
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_temporary_name_left_by_a_stopped_run_is_skipped_and_left_alone() {
        let root = TempDir::new("octet-agy-guarded-temp");
        let dir = root.path();
        std::fs::create_dir_all(dir).unwrap();
        let stale: Vec<PathBuf> = (100..103)
            .map(|n| dir.join(format!("record.json.{}.{n}.tmp", std::process::id())))
            .collect();
        for path in &stale {
            std::fs::write(path, "left behind").unwrap();
        }
        let mut counter = 100;
        write_private_with(dir, "record.json", b"new", || {
            counter += 1;
            counter - 1
        })
        .unwrap();
        assert_eq!(std::fs::read(dir.join("record.json")).unwrap(), b"new");
        for path in &stale {
            assert_eq!(std::fs::read(path).unwrap(), b"left behind");
        }
        assert_eq!(counter, 104, "the fourth name was free");
        assert!(
            !dir.join(format!("record.json.{}.103.tmp", std::process::id()))
                .exists()
        );
    }

    #[test]
    fn sixteen_taken_names_fail_the_write_without_removing_any() {
        let root = TempDir::new("octet-agy-guarded-taken");
        let dir = root.path();
        std::fs::create_dir_all(dir).unwrap();
        for n in 0..TEMP_TRIES as u64 {
            std::fs::write(
                dir.join(format!("record.json.{}.{n}.tmp", std::process::id())),
                "left behind",
            )
            .unwrap();
        }
        let mut counter = 0;
        let error = write_private_with(dir, "record.json", b"new", || {
            counter += 1;
            counter - 1
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
        assert!(!dir.join("record.json").exists());
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), TEMP_TRIES);
    }
}
