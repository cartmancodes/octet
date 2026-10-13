//! The run directory: each running terminal UI publishes a [`Registration`] there, beside its
//! socket, and the hub lists them.
use octet_store::{create_private, sync_directory};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// `<data dir>/run`.
pub fn run_dir(directory: &Path) -> PathBuf {
    directory.join("run")
}

/// What a running terminal UI publishes about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// `<pid>-<start unix secs>`.
    pub id: String,
    /// The terminal UI's process ID.
    pub pid: u32,
    /// When it started, in seconds since the Unix epoch.
    pub started: u64,
    /// Its bridge socket, `<run>/<pid>.sock`.
    pub socket: PathBuf,
    /// The provider's name.
    pub engine: String,
    /// The workspace it runs in.
    pub workspace: PathBuf,
    /// The model in use.
    pub model: String,
    /// The permission mode in use.
    pub mode: String,
    /// The vendor session ID; empty until the vendor names it.
    pub vendor_session: String,
    /// Every journal of this terminal UI, oldest first.
    pub journals: Vec<PathBuf>,
    /// Whether the hub's `start` launched it.
    pub from_phone: bool,
    /// The tmux session the hub's `start` launched it in.
    pub tmux: Option<String>,
}

impl Registration {
    /// The registration as the JSON object written to `<pid>.json`.
    pub fn to_json(&self) -> Value {
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let mut value = json!({
            "id": self.id,
            "pid": self.pid,
            "started": self.started,
            "socket": path(&self.socket),
            "engine": self.engine,
            "workspace": path(&self.workspace),
            "model": self.model,
            "mode": self.mode,
            "vendor_session": self.vendor_session,
            "journals": self.journals.iter().map(|j| path(j)).collect::<Vec<_>>(),
            "from_phone": self.from_phone,
        });
        if let Some(tmux) = &self.tmux {
            value["tmux"] = json!(tmux);
        }
        value
    }

    /// The registration `value` describes; `None` if a field is missing or mistyped.
    pub fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key)?.as_str().map(str::to_owned);
        let journals = value
            .get("journals")?
            .as_array()?
            .iter()
            .map(|j| j.as_str().map(PathBuf::from))
            .collect::<Option<Vec<_>>>()?;
        let tmux = match value.get("tmux") {
            None | Some(Value::Null) => None,
            Some(name) => Some(name.as_str()?.to_owned()),
        };
        Some(Self {
            id: text("id")?,
            pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
            started: value.get("started")?.as_u64()?,
            socket: text("socket")?.into(),
            engine: text("engine")?,
            workspace: text("workspace")?.into(),
            model: text("model")?,
            mode: text("mode")?,
            vendor_session: text("vendor_session")?,
            journals,
            from_phone: value.get("from_phone")?.as_bool()?,
            tmux,
        })
    }

    /// Writes `<run>/<pid>.json` atomically: an owner-only temp file, then a rename.
    ///
    /// # Errors
    ///
    /// Fails if the temp file cannot be written or renamed into place.
    pub async fn write(&self, run: &Path) -> std::io::Result<()> {
        let temp = run.join(format!("{}.json.tmp", self.pid));
        match tokio::fs::remove_file(&temp).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut file = create_private(&temp).await?;
        file.write_all(self.to_json().to_string().as_bytes())
            .await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, run.join(format!("{}.json", self.pid))).await?;
        sync_directory(run).await
    }

    /// Removes `<run>/<pid>.json`; a file already gone is fine.
    pub async fn remove(&self, run: &Path) {
        let _ = tokio::fs::remove_file(run.join(format!("{}.json", self.pid))).await;
    }
}

/// Every registration in `run`, oldest first; files that do not parse are skipped.
pub async fn registrations(run: &Path) -> Vec<Registration> {
    let Ok(mut entries) = tokio::fs::read_dir(run).await else {
        return Vec::new();
    };
    let mut found = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        if let Some(registration) = serde_json::from_slice(&bytes)
            .ok()
            .and_then(|v: Value| Registration::from_json(&v))
        {
            found.push(registration);
        }
    }
    found.sort_by_key(|a| (a.started, a.pid));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sample() -> Registration {
        Registration {
            id: "4242-1790000000".into(),
            pid: 4242,
            started: 1_790_000_000,
            socket: "/run/4242.sock".into(),
            engine: "claude".into(),
            workspace: "/work".into(),
            model: "opus".into(),
            mode: "ask".into(),
            vendor_session: "c-1".into(),
            journals: vec!["/j/a.jsonl".into(), "/j/b.jsonl".into()],
            from_phone: true,
            tmux: Some("octet-abc123".into()),
        }
    }

    #[tokio::test]
    async fn registration_round_trips_and_writes_privately() {
        let temp = octet_testkit::TempDir::new("octet-core-run");
        let run = run_dir(temp.path());
        octet_store::create_private_dir(&run).await.unwrap();
        let registration = sample();
        registration.write(&run).await.unwrap();
        // Writing again replaces it, and a stale temp file does not get in the way.
        std::fs::write(run.join("4242.json.tmp"), "stale").unwrap();
        registration.write(&run).await.unwrap();
        std::fs::write(run.join("x.json"), "garbage").unwrap();
        assert_eq!(
            registrations(&run).await,
            std::slice::from_ref(&registration)
        );
        let mode = std::fs::metadata(run.join("4242.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!run.join("4242.json.tmp").exists());
        registration.remove(&run).await;
        assert!(registrations(&run).await.is_empty());
        assert!(registrations(&run.join("missing")).await.is_empty());
    }

    #[test]
    fn from_json_rejects_missing_and_mistyped_fields() {
        let good = sample().to_json();
        assert_eq!(Registration::from_json(&good), Some(sample()));
        for key in good.as_object().unwrap().keys().filter(|k| *k != "tmux") {
            let mut missing = good.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert_eq!(Registration::from_json(&missing), None, "{key} missing");
            let mut mistyped = good.clone();
            mistyped[key] = json!(null);
            assert_eq!(Registration::from_json(&mistyped), None, "{key} null");
        }
    }

    #[test]
    fn the_tmux_session_is_optional_but_must_be_text() {
        let mut desk = sample();
        desk.tmux = None;
        let json = desk.to_json();
        assert!(json.get("tmux").is_none(), "{json}");
        assert_eq!(Registration::from_json(&json), Some(desk.clone()));
        let mut null = sample().to_json();
        null["tmux"] = json!(null);
        assert_eq!(Registration::from_json(&null), Some(desk));
        let mut mistyped = sample().to_json();
        mistyped["tmux"] = json!(7);
        assert_eq!(Registration::from_json(&mistyped), None);
        assert_eq!(sample().to_json()["tmux"], "octet-abc123");
    }
}
