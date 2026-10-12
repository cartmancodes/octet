//! Devices, pairing secrets, hub config and the audit log.
use octet_store::{create_private, create_private_dir, sync_directory};
use sha2::{Digest, Sha256};
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};

/// The loopback port when `hub.json` names none.
const DEFAULT_PORT: u16 = 47823;
/// How long a pairing secret works.
const SECRET_LIFETIME: Duration = Duration::from_mins(10);
/// Pairing secrets that can wait at once; the oldest give way.
const SECRETS_KEPT: usize = 16;
/// Longest device name or platform, in characters.
const NAME_LIMIT: usize = 64;
/// How stale `last_seen` may get on disk before a connection rewrites it.
const SEEN_WRITE_GAP: u64 = 60;
/// Shown when a pairing secret is wrong, used or old.
const SECRET_REFUSED: &str = "Pairing code already used or expired";

/// A paired phone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// The name the phone gave at pairing.
    pub name: String,
    /// The phone's platform.
    pub platform: String,
    /// The tailnet login seen at pairing.
    pub login: String,
    /// When it paired, in seconds since the epoch.
    pub created: u64,
    /// When it last connected, in seconds since the epoch.
    pub last_seen: u64,
    /// The hex SHA-256 of its token; the token itself is never stored.
    pub token_sha256: String,
}

/// The hub's settings from `hub.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubConfig {
    /// The loopback port.
    pub port: u16,
    /// The workspaces a phone may start sessions in.
    pub workspaces: Vec<PathBuf>,
}

/// A file's mtime and size.
type Stamp = (SystemTime, u64);

/// A pairing secret as the hub remembers it: its hash and when it was issued.
#[derive(Debug)]
struct Secret {
    sha256: String,
    issued: SystemTime,
}

/// The hub's files in the data directory: `devices.json`, `hub.json` and
/// `hub-audit.jsonl`, all owner-only.
#[derive(Debug)]
pub struct Store {
    directory: PathBuf,
    devices: Vec<Device>,
    config: HubConfig,
    secrets: Vec<Secret>,
    /// The mtime and size of `devices.json` as this store last wrote or read
    /// it; the size catches a change within the clock's granularity.
    stamp: Option<Stamp>,
}

impl Store {
    /// Opens the store in `directory`, creating it owner-only if missing.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created or `devices.json` or
    /// `hub.json` cannot be read or are malformed.
    pub async fn open(directory: &Path) -> io::Result<Store> {
        create_private_dir(directory).await?;
        let (devices, stamp) = read_devices(directory).await?;
        Ok(Store {
            directory: directory.to_owned(),
            devices,
            config: read_config(directory).await?,
            secrets: Vec::new(),
            stamp,
        })
    }

    /// A new pairing secret: 32 random bytes in hex, good for 10 minutes and
    /// one pairing. Only its hash is kept.
    ///
    /// # Panics
    ///
    /// Panics if the operating system has no random bytes to give.
    pub fn pairing_secret(&mut self, now: SystemTime) -> String {
        let secret = random_hex();
        self.secrets.retain(|kept| !expired(kept.issued, now));
        if self.secrets.len() >= SECRETS_KEPT {
            self.secrets.remove(0);
        }
        self.secrets.push(Secret {
            sha256: sha256_hex(&secret),
            issued: now,
        });
        secret
    }

    /// Pairs a phone: spends `secret` and returns the new device token, which
    /// is shown once and not stored, and the device. A refusal that is the
    /// phone's to fix leaves the secret unspent.
    ///
    /// # Errors
    ///
    /// Fails with the reason to show the phone when the secret is unknown,
    /// used or older than 10 minutes, a field is empty or too long, the name
    /// is taken, or the device cannot be saved.
    pub async fn pair(
        &mut self,
        secret: &str,
        name: &str,
        platform: &str,
        login: &str,
        now: SystemTime,
    ) -> Result<(String, Device), &'static str> {
        let hash = sha256_hex(secret);
        let position = self
            .secrets
            .iter()
            .position(|kept| same(&kept.sha256, &hash) && !expired(kept.issued, now))
            .ok_or(SECRET_REFUSED)?;
        if name.is_empty() || name.chars().count() > NAME_LIMIT {
            return Err("Device name must be 1 to 64 characters");
        }
        if platform.is_empty() || platform.chars().count() > NAME_LIMIT {
            return Err("Device platform must be 1 to 64 characters");
        }
        if login.is_empty() {
            return Err("Tailnet login is missing");
        }
        if self.devices.iter().any(|device| device.name == name) {
            return Err("A device with that name is already paired");
        }
        self.secrets.swap_remove(position);
        let token = random_hex();
        let seconds = seconds(now);
        let device = Device {
            name: name.to_owned(),
            platform: platform.to_owned(),
            login: login.to_owned(),
            created: seconds,
            last_seen: seconds,
            token_sha256: sha256_hex(&token),
        };
        self.devices.push(device.clone());
        if self.save().await.is_err() {
            self.devices.pop();
            return Err("Could not save the device");
        }
        Ok((token, device))
    }

    /// The device whose token this is, if it also connects from the login it
    /// paired with. Records the time it was seen.
    pub async fn authenticate(
        &mut self,
        token: &str,
        login: &str,
        now: SystemTime,
    ) -> Option<Device> {
        let hash = sha256_hex(token);
        // Every device is compared, so the time taken does not say which matched.
        let mut found = None;
        for (index, device) in self.devices.iter().enumerate() {
            if same(&device.token_sha256, &hash) {
                found = Some(index);
            }
        }
        let device = self.devices.get_mut(found?)?;
        if device.login != login {
            return None;
        }
        let seen = seconds(now).max(device.last_seen);
        let stale = seen >= device.last_seen + SEEN_WRITE_GAP;
        device.last_seen = seen;
        let device = device.clone();
        if stale {
            // A failed write only leaves the file's last_seen older.
            let _ = self.save().await;
        }
        Some(device)
    }

    /// Deletes the device called `name`; false if there is none or the file
    /// cannot be written.
    pub async fn revoke(&mut self, name: &str) -> bool {
        let before = self.devices.clone();
        self.devices.retain(|device| device.name != name);
        if self.devices.len() == before.len() {
            return false;
        }
        if self.save().await.is_err() {
            self.devices = before;
            return false;
        }
        true
    }

    /// The paired devices.
    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    /// Re-reads `devices.json` if another process (`octet hub revoke`)
    /// changed it; true if the devices were replaced.
    pub async fn reload_if_changed(&mut self) -> bool {
        let stamp = file_stamp(&self.directory.join("devices.json")).await;
        if stamp == self.stamp {
            return false;
        }
        match read_devices(&self.directory).await {
            Ok((devices, stamp)) => {
                self.devices = devices;
                self.stamp = stamp;
                true
            }
            Err(_) => false,
        }
    }

    /// The hub's settings.
    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    /// Appends one `{time, device, session, what}` line to `hub-audit.jsonl`.
    /// `what` names the action, never its content: no prompt text, tokens or
    /// secrets. A line that cannot be written is dropped.
    pub async fn audit(&self, device: &str, session: &str, what: &str) {
        let line = serde_json::json!({
            "time": seconds(SystemTime::now()),
            "device": device,
            "session": session,
            "what": what,
        });
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            return;
        };
        bytes.push(b'\n');
        let Ok(mut file) = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.directory.join("hub-audit.jsonl"))
            .await
        else {
            return;
        };
        // One write per line keeps lines whole when the hub and a CLI append.
        if file.write_all(&bytes).await.is_ok() {
            let _ = file.flush().await;
        }
    }

    /// Writes `devices.json` through a temp file and a rename.
    async fn save(&mut self) -> io::Result<()> {
        let devices: Vec<_> = self
            .devices
            .iter()
            .map(|device| {
                serde_json::json!({
                    "name": device.name,
                    "platform": device.platform,
                    "login": device.login,
                    "created": device.created,
                    "last_seen": device.last_seen,
                    "token_sha256": device.token_sha256,
                })
            })
            .collect();
        let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({ "devices": devices }))?;
        bytes.push(b'\n');
        write_private(&self.directory, "devices.json", &bytes).await?;
        self.stamp = file_stamp(&self.directory.join("devices.json")).await;
        Ok(())
    }
}

/// Writes `name` in `directory` owner-only: a temp file, synced, then renamed.
async fn write_private(directory: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = directory.join(format!("{name}.{}.tmp", std::process::id()));
    match fs::remove_file(&temporary).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut file = create_private(&temporary).await?;
    let written = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await
    }
    .await;
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary).await;
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, directory.join(name)).await {
        let _ = fs::remove_file(&temporary).await;
        return Err(error);
    }
    sync_directory(directory).await
}

async fn file_stamp(path: &Path) -> Option<Stamp> {
    let metadata = fs::metadata(path).await.ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

/// The devices in `devices.json` (none if it is missing) and its stamp.
async fn read_devices(directory: &Path) -> io::Result<(Vec<Device>, Option<Stamp>)> {
    let path = directory.join("devices.json");
    let bytes = match fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(error) => return Err(error),
    };
    let stamp = file_stamp(&path).await;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("devices.json is not valid JSON"))?;
    let list = value["devices"]
        .as_array()
        .ok_or_else(|| invalid("devices.json has no devices list"))?;
    let devices = list
        .iter()
        .map(|entry| {
            let text = |key: &str| entry[key].as_str().map(str::to_owned);
            Some(Device {
                name: text("name")?,
                platform: text("platform")?,
                login: text("login")?,
                created: entry["created"].as_u64()?,
                last_seen: entry["last_seen"].as_u64()?,
                token_sha256: text("token_sha256")?,
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid("devices.json has a malformed device"))?;
    Ok((devices, stamp))
}

/// The settings in `hub.json`; defaults if it is missing.
async fn read_config(directory: &Path) -> io::Result<HubConfig> {
    let mut config = HubConfig {
        port: DEFAULT_PORT,
        workspaces: Vec::new(),
    };
    let bytes = match fs::read(directory.join("hub.json")).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(config),
        Err(error) => return Err(error),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("hub.json is not valid JSON"))?;
    if !value["port"].is_null() {
        config.port = value["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid("hub.json port must be 1 to 65535"))?;
    }
    if !value["workspaces"].is_null() {
        config.workspaces = value["workspaces"]
            .as_array()
            .and_then(|list| {
                list.iter()
                    .map(|path| path.as_str().map(PathBuf::from))
                    .collect()
            })
            .ok_or_else(|| invalid("hub.json workspaces must be a list of paths"))?;
    }
    Ok(config)
}

/// Whether a secret issued at `issued` is past its 10 minutes at `now`.
fn expired(issued: SystemTime, now: SystemTime) -> bool {
    now.duration_since(issued)
        .is_ok_and(|age| age > SECRET_LIFETIME)
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// 32 random bytes as 64 hex digits.
fn random_hex() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system gave no random bytes");
    hex(&bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn sha256_hex(text: &str) -> String {
    hex(Sha256::digest(text.as_bytes()).as_slice())
}

/// Equality that takes the same time wherever the texts differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0_u8, |difference, (x, y)| difference | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::TempDir;
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    const LOGIN: &str = "me@example.com";

    async fn store(dir: &TempDir) -> Store {
        Store::open(dir.path()).await.unwrap()
    }

    fn epoch(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    async fn paired(store: &mut Store, name: &str, now: SystemTime) -> String {
        let secret = store.pairing_secret(now);
        store
            .pair(&secret, name, "ios", LOGIN, now)
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn a_pairing_secret_works_once_and_expires() {
        let dir = TempDir::new("hub-pair");
        let mut store = store(&dir).await;
        let now = epoch(1_000_000);
        let secret = store.pairing_secret(now);
        assert_eq!(secret.len(), 64);
        let (token, device) = store
            .pair(&secret, "Gemini 10", "ios", LOGIN, now)
            .await
            .unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(
            (
                device.name.as_str(),
                device.platform.as_str(),
                device.login.as_str()
            ),
            ("Gemini 10", "ios", LOGIN)
        );
        assert_eq!((device.created, device.last_seen), (1_000_000, 1_000_000));
        let refusal = Err("Pairing code already used or expired");
        assert_eq!(
            store
                .pair(&secret, "Again", "ios", LOGIN, now)
                .await
                .map(|_| ()),
            refusal
        );
        let stale = store.pairing_secret(now);
        let late = now + Duration::from_mins(11);
        assert_eq!(
            store
                .pair(&stale, "Late", "ios", LOGIN, late)
                .await
                .map(|_| ()),
            refusal
        );
        let nearly = store.pairing_secret(now);
        let edge = now + Duration::from_mins(10);
        assert!(
            store
                .pair(&nearly, "Edge", "ios", LOGIN, edge)
                .await
                .is_ok()
        );
        assert_eq!(
            store
                .pair("0", "Guess", "ios", LOGIN, now)
                .await
                .map(|_| ()),
            refusal
        );
        assert_eq!(store.devices().len(), 2);
    }

    #[tokio::test]
    async fn a_pairing_refusal_keeps_the_secret_for_a_retry() {
        let dir = TempDir::new("hub-retry");
        let mut store = store(&dir).await;
        let now = epoch(5);
        let first = paired(&mut store, "Phone", now).await;
        assert_eq!(first.len(), 64);
        let secret = store.pairing_secret(now);
        assert!(
            store
                .pair(&secret, "Phone", "ios", LOGIN, now)
                .await
                .is_err()
        );
        assert!(store.pair(&secret, "", "ios", LOGIN, now).await.is_err());
        assert!(store.pair(&secret, "Other", "ios", "", now).await.is_err());
        assert!(
            store
                .pair(&secret, "Other", "ios", LOGIN, now)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_token_needs_its_pairing_login() {
        let dir = TempDir::new("hub-login");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(100)).await;
        let device = store.authenticate(&token, LOGIN, epoch(200)).await.unwrap();
        assert_eq!(device.name, "Gemini 10");
        assert_eq!(device.last_seen, 200);
        assert_eq!(store.devices()[0].last_seen, 200);
        assert!(
            store
                .authenticate(&token, "other@example.com", epoch(300))
                .await
                .is_none()
        );
        assert!(
            store
                .authenticate(&"0".repeat(64), LOGIN, epoch(300))
                .await
                .is_none()
        );
        assert!(store.authenticate("", LOGIN, epoch(300)).await.is_none());
        assert_eq!(store.devices()[0].last_seen, 200);
    }

    #[tokio::test]
    async fn revoking_removes_a_device_and_the_file_says_so() {
        let dir = TempDir::new("hub-revoke");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(1)).await;
        let file = dir.path().join("devices.json");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("Gemini 10") && !text.contains(&token));
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(Store::open(dir.path()).await.unwrap().devices().len(), 1);
        assert!(!store.revoke("Nobody").await);
        assert!(store.revoke("Gemini 10").await);
        assert!(store.authenticate(&token, LOGIN, epoch(2)).await.is_none());
        let reopened = Store::open(dir.path()).await.unwrap();
        assert!(reopened.devices().is_empty());
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!std::fs::read_to_string(&file).unwrap().contains(&token));
    }

    #[tokio::test]
    async fn a_revoke_from_another_process_is_seen_on_reload() {
        let dir = TempDir::new("hub-reload");
        let mut hub = store(&dir).await;
        let token = paired(&mut hub, "Gemini 10", epoch(1)).await;
        assert!(!hub.reload_if_changed().await);
        let mut cli = Store::open(dir.path()).await.unwrap();
        assert!(cli.revoke("Gemini 10").await);
        assert!(hub.reload_if_changed().await);
        assert!(hub.devices().is_empty());
        assert!(hub.authenticate(&token, LOGIN, epoch(2)).await.is_none());
        assert!(!hub.reload_if_changed().await);
    }

    #[tokio::test]
    async fn the_config_defaults_and_reads_hub_json() {
        let dir = TempDir::new("hub-config");
        let store = store(&dir).await;
        assert_eq!(
            store.config(),
            &HubConfig {
                port: 47823,
                workspaces: Vec::new()
            }
        );
        std::fs::write(
            dir.path().join("hub.json"),
            r#"{"port":50000,"workspaces":["/a","/b"]}"#,
        )
        .unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        assert_eq!(store.config().port, 50000);
        assert_eq!(
            store.config().workspaces,
            [PathBuf::from("/a"), PathBuf::from("/b")]
        );
        std::fs::write(dir.path().join("hub.json"), r#"{"port":70000}"#).unwrap();
        assert!(Store::open(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn audit_lines_never_hold_prompt_text() {
        let dir = TempDir::new("hub-audit");
        let store = store(&dir).await;
        store.audit("Gemini 10", "s", "prompt").await;
        store.audit("Gemini 10", "s", "answer").await;
        let file = dir.path().join("hub-audit.jsonl");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        let object = lines[0].as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["device", "session", "time", "what"]);
        assert_eq!(
            (
                object["device"].as_str(),
                object["session"].as_str(),
                object["what"].as_str()
            ),
            (Some("Gemini 10"), Some("s"), Some("prompt"))
        );
        assert!(object["time"].is_u64());
    }

    #[tokio::test]
    async fn secrets_and_tokens_never_reach_the_audit_file() {
        let dir = TempDir::new("hub-nolog");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(1)).await;
        store.audit("Gemini 10", "s", "paired").await;
        let text = std::fs::read_to_string(dir.path().join("hub-audit.jsonl")).unwrap();
        assert!(!text.contains(&token));
        assert!(!format!("{store:?}").contains(&token));
    }
}
